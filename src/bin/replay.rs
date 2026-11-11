//! lgh-replay: synthetic or recorded Semtech UDP traffic for the demo and for load tests.
//!
//!   lgh-replay forwarder --target exporter:1701 [--rate 3] [--stat-interval 10] [--file traffic.jsonl]
//!   lgh-replay bridge --listen 0.0.0.0:1700 [--downlink-every 6] [--outage-after 300 --outage-for 60]
//!
//! `forwarder` behaves like a packet forwarder (two sockets, PUSH_DATA with rxpk and stat,
//! PULL_DATA keepalives, TX_ACK for each PULL_RESP). `bridge` acknowledges like a gateway
//! bridge and orders a downlink every few seconds. The traffic is synthetic: devices have a
//! fixed link strength, pick the lowest SF that leaves 10 dB of margin, and fade slowly.

use std::fs;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lgh::semtech::{self, build_datagram, Message};

const CHANNELS_MHZ: [f64; 8] = [868.1, 868.3, 868.5, 867.1, 867.3, 867.5, 867.7, 867.9];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        // sum of 4 uniforms, centred: good enough for jitter
        (0..4).map(|_| self.unit()).sum::<f64>() - 2.0
    }
}

struct Device {
    channel: usize,
    /// RSSI at SF-independent reference, dBm.
    rssi: f64,
    payload: u32,
}

fn sensitivity(sf: u8) -> f64 {
    -123.0 - 2.5 * f64::from(sf - 7)
}

fn pick_sf(rssi: f64) -> u8 {
    (7..=12)
        .find(|sf| rssi - sensitivity(*sf) >= 10.0)
        .unwrap_or(12)
}

fn utc_string(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} GMT",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn arg_num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    match arg_value(args, name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("bad value for {name}: {v:?}")),
    }
}

fn parse_eui(s: &str) -> Result<[u8; 8], String> {
    if s.len() != 16 || !s.is_ascii() {
        return Err(format!("--eui wants 16 hex digits, got {s:?}"));
    }
    let mut out = [0u8; 8];
    for (i, b) in out.iter_mut().enumerate() {
        *b =
            u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| format!("bad --eui {s:?}"))?;
    }
    Ok(out)
}

fn resolve(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| format!("{s}: {e}"))?
        .next()
        .ok_or_else(|| format!("{s}: no address"))
}

#[derive(Default)]
struct Counters {
    sent_push: AtomicU64,
    push_acked: AtomicU64,
    dwnb: AtomicU64,
    txnb: AtomicU64,
}

fn forwarder(args: &[String]) -> Result<(), String> {
    let target = resolve(&arg_value(args, "--target").ok_or("--target HOST:PORT is required")?)?;
    let eui = parse_eui(&arg_value(args, "--eui").unwrap_or_else(|| "aa555a0000000101".into()))?;
    let rate: f64 = arg_num(args, "--rate", 3.0)?;
    let stat_every: f64 = arg_num(args, "--stat-interval", 10.0)?;
    let keepalive: f64 = arg_num(args, "--keepalive", 10.0)?;
    let duration: f64 = arg_num(args, "--duration", 0.0)?;
    let seed: u64 = arg_num(args, "--seed", 7)?;
    let devices_n: usize = arg_num(args, "--devices", 40)?;
    let file = arg_value(args, "--file");
    if rate <= 0.0 || stat_every <= 0.0 || keepalive <= 0.0 {
        return Err("--rate, --stat-interval and --keepalive must be > 0".into());
    }

    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let up = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    up.connect(target).map_err(|e| e.to_string())?;
    let down = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    down.connect(target).map_err(|e| e.to_string())?;
    let counters = Arc::new(Counters::default());

    // Uplink socket reader: PUSH_ACKs.
    {
        let up = up.try_clone().map_err(|e| e.to_string())?;
        let c = counters.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok(n) = up.recv(&mut buf) {
                if n >= 4 && buf[3] == semtech::PUSH_ACK {
                    c.push_acked.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    // Downlink socket: keepalives, PULL_RESP -> TX_ACK.
    {
        let down = down.try_clone().map_err(|e| e.to_string())?;
        let c = counters.clone();
        let mut ka_rng = Rng(seed ^ 0x9e37_79b9);
        thread::spawn(move || {
            let mut token = 1u16;
            let mut last = Instant::now() - Duration::from_secs(3600);
            let mut buf = [0u8; 4096];
            let _ = down.set_read_timeout(Some(Duration::from_millis(200)));
            loop {
                if last.elapsed().as_secs_f64() >= keepalive {
                    token = token.wrapping_add(1);
                    let _ = down.send(&build_datagram(
                        2,
                        token,
                        semtech::PULL_DATA,
                        Some(&eui),
                        "",
                    ));
                    last = Instant::now();
                }
                if let Ok(n) = down.recv(&mut buf) {
                    if let Ok(Message::PullResp { .. }) = semtech::parse_datagram(&buf[..n]) {
                        c.dwnb.fetch_add(1, Ordering::Relaxed);
                        let t = u16::from(buf[1]) << 8 | u16::from(buf[2]);
                        let err = if ka_rng.unit() < 0.07 {
                            "TOO_LATE"
                        } else {
                            "NONE"
                        };
                        if err == "NONE" {
                            c.txnb.fetch_add(1, Ordering::Relaxed);
                        }
                        let body = if err == "NONE" {
                            String::new()
                        } else {
                            format!("{{\"txpk_ack\":{{\"error\":\"{err}\"}}}}")
                        };
                        let _ =
                            down.send(&build_datagram(2, t, semtech::TX_ACK, Some(&eui), &body));
                    }
                }
            }
        });
    }

    let mut rng = Rng(seed.max(1));
    let devices: Vec<Device> = (0..devices_n)
        .map(|_| Device {
            channel: (rng.next() % 8) as usize,
            // Skewed towards weak links, like a gateway with a few near and many far devices.
            rssi: -128.0 + 46.0 * rng.unit().powf(1.2),
            payload: 13 + (rng.next() % 30) as u32,
        })
        .collect();

    let replay_lines: Option<Vec<String>> = match &file {
        Some(p) => Some(
            fs::read_to_string(p)
                .map_err(|e| format!("{p}: {e}"))?
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(String::from)
                .collect(),
        ),
        None => None,
    };
    if replay_lines.as_ref().is_some_and(Vec::is_empty) {
        return Err("--file has no lines".into());
    }

    eprintln!(
        "lgh-replay forwarder -> {target} ({})",
        file.as_deref().unwrap_or("synthetic")
    );
    let t0 = Instant::now();
    let mut token = 0u16;
    let mut next_stat = stat_every;
    let (mut rx_ok, mut rx_bad) = (0u64, 0u64);
    let mut replay_i = 0usize;
    let mut last_acked = 0u64;
    let mut last_sent = 0u64;
    let (mut last_dwnb, mut last_txnb) = (0u64, 0u64);
    let tick = 0.1f64;

    loop {
        let t = t0.elapsed().as_secs_f64();
        if duration > 0.0 && t >= duration {
            break;
        }
        // Scenario: a CRC-error burst between 55 % and 75 % of every 10-minute cycle.
        let phase = (t % 600.0) / 600.0;
        let bad_share = if (0.55..0.75).contains(&phase) {
            0.30
        } else {
            0.03
        };

        if let Some(lines) = &replay_lines {
            // Recorded mode: one line per tick-interval derived from --rate.
            if rng.unit() < rate * tick {
                token = token.wrapping_add(1);
                let line = &lines[replay_i % lines.len()];
                replay_i += 1;
                let _ = up.send(&build_datagram(
                    2,
                    token,
                    semtech::PUSH_DATA,
                    Some(&eui),
                    line,
                ));
                counters.sent_push.fetch_add(1, Ordering::Relaxed);
            }
        } else if rng.unit() < rate * tick {
            let d = &devices[(rng.next() as usize) % devices.len()];
            // slow drift (one device group fades during the cycle) plus fast noise
            let drift =
                -6.0 * (phase * std::f64::consts::TAU).sin().max(0.0) * (d.channel as f64 / 7.0);
            let rssi = (d.rssi + drift + rng.gauss() * 2.0).round();
            let sf = pick_sf(d.rssi + drift);
            // Noise floor about -117 dBm for 125 kHz; the concentrator reports SNR up to roughly +12 dB.
            let snr = (rssi + 117.0 + rng.gauss()).clamp(-20.0, 12.5);
            if rng.unit() < bad_share {
                // Heard but failed CRC: counted by the concentrator, not forwarded (forward_crc_error=false).
                rx_bad += 1;
            } else {
                rx_ok += 1;
                let json = format!(
                    "{{\"rxpk\":[{{\"tmst\":{},\"chan\":{},\"rfch\":0,\"freq\":{},\"stat\":1,\"modu\":\"LORA\",\"datr\":\"SF{sf}BW125\",\"codr\":\"4/5\",\"lsnr\":{snr:.1},\"rssi\":{rssi},\"size\":{}}}]}}",
                    (t * 1e6) as u64,
                    d.channel,
                    CHANNELS_MHZ[d.channel],
                    d.payload
                );
                token = token.wrapping_add(1);
                let _ = up.send(&build_datagram(
                    2,
                    token,
                    semtech::PUSH_DATA,
                    Some(&eui),
                    &json,
                ));
                counters.sent_push.fetch_add(1, Ordering::Relaxed);
            }
        }

        if t >= next_stat {
            next_stat += stat_every;
            let sent = counters.sent_push.load(Ordering::Relaxed);
            let acked = counters.push_acked.load(Ordering::Relaxed);
            let ackr = if sent > last_sent {
                100.0 * (acked - last_acked).min(sent - last_sent) as f64
                    / (sent - last_sent) as f64
            } else {
                100.0
            };
            let (dw, tx) = (
                counters.dwnb.load(Ordering::Relaxed),
                counters.txnb.load(Ordering::Relaxed),
            );
            let json = format!(
                "{{\"stat\":{{\"time\":\"{}\",\"rxnb\":{},\"rxok\":{rx_ok},\"rxfw\":{rx_ok},\"ackr\":{ackr:.1},\"dwnb\":{},\"txnb\":{}}}}}",
                utc_string(unix_now()),
                rx_ok + rx_bad,
                dw - last_dwnb,
                tx - last_txnb
            );
            token = token.wrapping_add(1);
            let _ = up.send(&build_datagram(
                2,
                token,
                semtech::PUSH_DATA,
                Some(&eui),
                &json,
            ));
            counters.sent_push.fetch_add(1, Ordering::Relaxed);
            last_sent = sent + 1;
            last_acked = acked;
            last_dwnb = dw;
            last_txnb = tx;
            rx_ok = 0;
            rx_bad = 0;
        }
        thread::sleep(Duration::from_secs_f64(tick));
    }
    Ok(())
}

fn bridge(args: &[String]) -> Result<(), String> {
    let listen = resolve(&arg_value(args, "--listen").unwrap_or_else(|| "0.0.0.0:1700".into()))?;
    let dl_every: f64 = arg_num(args, "--downlink-every", 6.0)?;
    let outage_after: f64 = arg_num(args, "--outage-after", 0.0)?;
    let outage_for: f64 = arg_num(args, "--outage-for", 0.0)?;
    let seed: u64 = arg_num(args, "--seed", 11)?;
    let sock = UdpSocket::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    eprintln!("lgh-replay bridge on {listen}");
    let t0 = Instant::now();
    let mut rng = Rng(seed.max(1));
    let mut pull_peer: Option<SocketAddr> = None;
    let mut next_dl = dl_every;
    let mut token = 1000u16;
    let mut buf = [0u8; 8192];
    loop {
        let t = t0.elapsed().as_secs_f64();
        // Outage cycle: repeat every `outage_after + outage_for` seconds when configured.
        let in_outage = outage_for > 0.0 && {
            let cycle = outage_after + outage_for;
            let p = t % cycle;
            p >= outage_after
        };
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if in_outage || n < 4 {
                continue;
            }
            match buf[3] {
                semtech::PUSH_DATA => {
                    let _ = sock.send_to(&[buf[0], buf[1], buf[2], semtech::PUSH_ACK], from);
                }
                semtech::PULL_DATA => {
                    pull_peer = Some(from);
                    let _ = sock.send_to(&[buf[0], buf[1], buf[2], semtech::PULL_ACK], from);
                }
                _ => {}
            }
        }
        if t >= next_dl {
            next_dl += dl_every;
            if let (Some(peer), false) = (pull_peer, in_outage) {
                token = token.wrapping_add(1);
                // Alternate RX2 (869.525 MHz, g3) with RX1 on 868.1 (g1) at different SFs.
                let json = match rng.next() % 3 {
                    0 => {
                        r#"{"txpk":{"imme":false,"tmst":1000000,"freq":869.525,"rfch":0,"powe":27,"modu":"LORA","datr":"SF9BW125","codr":"4/5","ipol":true,"prea":8,"size":33,"ncrc":true,"data":"IMZQ"}}"#
                    }
                    1 => {
                        r#"{"txpk":{"imme":false,"tmst":1000000,"freq":868.1,"rfch":0,"powe":14,"modu":"LORA","datr":"SF7BW125","codr":"4/5","ipol":true,"prea":8,"size":20,"ncrc":true,"data":"IMZQ"}}"#
                    }
                    _ => {
                        r#"{"txpk":{"imme":false,"tmst":1000000,"freq":868.3,"rfch":0,"powe":14,"modu":"LORA","datr":"SF10BW125","codr":"4/5","ipol":true,"prea":8,"size":26,"ncrc":true,"data":"IMZQ"}}"#
                    }
                };
                let _ = sock.send_to(
                    &build_datagram(2, token, semtech::PULL_RESP, None, json),
                    peer,
                );
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("forwarder") => forwarder(&args[1..]),
        Some("bridge") => bridge(&args[1..]),
        _ => Err("usage: lgh-replay forwarder|bridge [options] (see the source header)".into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

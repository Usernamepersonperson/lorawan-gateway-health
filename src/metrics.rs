//! Metric state and Prometheus text exposition (format version 0.0.4).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::airtime::{rxpk_airtime_s, txpk_airtime_s, Region};
use crate::semtech::{self, Crc, DataRate, Message, Stat};
use crate::sys::SysSnapshot;

/// RSSI histogram upper bounds in dBm.
pub const RSSI_BUCKETS: &[f64] = &[
    -135.0, -130.0, -125.0, -120.0, -115.0, -110.0, -105.0, -100.0, -95.0, -90.0, -85.0, -80.0,
    -70.0, -60.0, -50.0,
];
/// SNR histogram upper bounds in dB. LoRa demodulates down to about -20 dB at SF12.
pub const SNR_BUCKETS: &[f64] = &[
    -20.0, -15.0, -10.0, -7.5, -5.0, -2.5, 0.0, 2.5, 5.0, 7.5, 10.0, 12.5,
];

/// Upper bound on distinct (channel, SF, BW) series; further ones fold into "other".
pub const MAX_RX_SERIES: usize = 256;
/// Sliding window for duty-cycle accounting.
pub const DUTY_WINDOW_S: f64 = 3600.0;
/// Number of forwarder `stat` reports the CRC-error-ratio gauge averages over.
pub const STAT_WINDOW: usize = 10;

/// Two clocks: monotonic seconds (for windows) and Unix seconds (for "last seen" timestamps).
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub mono: f64,
    pub unix: f64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub region: Region,
    /// A bridge is "up" if a PULL_ACK arrived within this many seconds.
    pub bridge_timeout_s: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            region: Region::Eu868,
            bridge_timeout_s: 60.0,
        }
    }
}

#[derive(Debug, Clone)]
struct Hist {
    /// Non-cumulative counts, one per bound plus the +Inf bucket.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Hist {
    fn new(bounds: &[f64]) -> Hist {
        Hist {
            counts: vec![0; bounds.len() + 1],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, bounds: &[f64], v: f64) {
        if !v.is_finite() {
            return;
        }
        let i = bounds.iter().position(|b| v <= *b).unwrap_or(bounds.len());
        self.counts[i] += 1;
        self.sum += v;
        self.count += 1;
    }
}

/// (frequency in kHz, spreading factor or 0 for FSK, bandwidth in kHz). All-zero = "other".
type RxKey = (u32, u8, u32);

struct RxSeries {
    crc: [u64; 3],
    rssi: Hist,
    snr: Hist,
    airtime_s: f64,
}

impl RxSeries {
    fn new() -> RxSeries {
        RxSeries {
            crc: [0; 3],
            rssi: Hist::new(RSSI_BUCKETS),
            snr: Hist::new(SNR_BUCKETS),
            airtime_s: 0.0,
        }
    }
}

#[derive(Default)]
struct MqttProbe {
    reachable: bool,
    connect_s: f64,
    checked_unix: f64,
}

#[derive(Default)]
struct Inner {
    datagrams: [u64; 6],
    parse_errors: BTreeMap<&'static str, u64>,
    rxpk_errors: u64,
    relay_errors: BTreeMap<&'static str, u64>,
    gateway_eui: Option<String>,

    rx: BTreeMap<RxKey, RxSeries>,

    stat_total: Stat,
    stat_reports: u64,
    stat_last_unix: Option<f64>,
    stat_last_ackr: Option<f64>,
    stat_window: VecDeque<(u64, u64)>,

    tx_requests: u64,
    tx_unparsed: u64,
    tx_airtime_s: f64,
    tx_ack: BTreeMap<String, u64>,
    /// (monotonic time, airtime seconds, band index)
    duty: VecDeque<(f64, f64, usize)>,
    /// Lifetime airtime per band, for the counter.
    band_airtime_s: BTreeMap<usize, f64>,

    forwarder_last_seen_unix: Option<f64>,
    last_pull_ack_mono: Option<f64>,
    last_pull_ack_unix: Option<f64>,
    last_push_ack_unix: Option<f64>,

    mqtt: Option<MqttProbe>,
}

pub struct Metrics {
    cfg: Config,
    inner: Mutex<Inner>,
}

fn kind_index(k: semtech::Kind) -> usize {
    match k {
        semtech::Kind::PushData => 0,
        semtech::Kind::PushAck => 1,
        semtech::Kind::PullData => 2,
        semtech::Kind::PullResp => 3,
        semtech::Kind::PullAck => 4,
        semtech::Kind::TxAck => 5,
    }
}

const KIND_NAMES: [&str; 6] = [
    "push_data",
    "push_ack",
    "pull_data",
    "pull_resp",
    "pull_ack",
    "tx_ack",
];

impl Metrics {
    pub fn new(cfg: Config) -> Metrics {
        Metrics {
            cfg,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another thread panicked mid-update; counters stay usable.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Parse and record one datagram seen on the tap. Never panics on bad input.
    pub fn observe(&self, data: &[u8], now: Now) {
        match semtech::parse_datagram(data) {
            Ok(msg) => self.record(&msg, now),
            Err(e) => {
                *self.lock().parse_errors.entry(e.label()).or_insert(0) += 1;
            }
        }
    }

    pub fn relay_error(&self, kind: &'static str) {
        *self.lock().relay_errors.entry(kind).or_insert(0) += 1;
    }

    pub fn set_mqtt_probe(&self, reachable: bool, connect_s: f64, checked_unix: f64) {
        self.lock().mqtt = Some(MqttProbe {
            reachable,
            connect_s,
            checked_unix,
        });
    }

    pub fn record(&self, msg: &Message, now: Now) {
        let mut g = self.lock();
        g.datagrams[kind_index(msg.kind())] += 1;
        match msg {
            Message::PushData {
                eui,
                stat,
                rxpk,
                rxpk_errors,
            } => {
                g.gateway_eui = Some(semtech::eui_hex(eui));
                g.forwarder_last_seen_unix = Some(now.unix);
                g.rxpk_errors += *rxpk_errors as u64;
                if let Some(s) = stat {
                    g.record_stat(s, now);
                }
                for p in rxpk {
                    g.record_rxpk(p);
                }
            }
            Message::PullData { eui } => {
                g.gateway_eui = Some(semtech::eui_hex(eui));
                g.forwarder_last_seen_unix = Some(now.unix);
            }
            Message::PushAck => g.last_push_ack_unix = Some(now.unix),
            Message::PullAck => {
                g.last_pull_ack_mono = Some(now.mono);
                g.last_pull_ack_unix = Some(now.unix);
            }
            Message::PullResp { txpk } => {
                g.tx_requests += 1;
                match txpk
                    .as_ref()
                    .and_then(|t| txpk_airtime_s(t).map(|a| (t, a)))
                {
                    Some((t, air)) => {
                        let band = self.cfg.region.band_index(t.freq_mhz);
                        g.tx_airtime_s += air;
                        *g.band_airtime_s.entry(band).or_insert(0.0) += air;
                        g.duty.push_back((now.mono, air, band));
                    }
                    None => g.tx_unparsed += 1,
                }
                g.prune_duty(now.mono);
            }
            Message::TxAck { eui, error } => {
                g.gateway_eui = Some(semtech::eui_hex(eui));
                *g.tx_ack.entry(error.clone()).or_insert(0) += 1;
            }
        }
    }

    /// Render all families. `sys` carries the host-level readings.
    pub fn render(&self, now: Now, sys: &SysSnapshot) -> String {
        let mut g = self.lock();
        g.prune_duty(now.mono);
        let mut o = String::with_capacity(16 * 1024);

        fam(
            &mut o,
            "lgh_exporter_build_info",
            "Exporter version.",
            "gauge",
        );
        sample(
            &mut o,
            "lgh_exporter_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))],
            "1",
        );
        if let Some(eui) = &g.gateway_eui {
            fam(
                &mut o,
                "lgh_gateway_info",
                "Gateway EUI taken from the forwarder datagrams.",
                "gauge",
            );
            sample(&mut o, "lgh_gateway_info", &[("gateway_eui", eui)], "1");
        }

        // ---- datagram bookkeeping
        fam(
            &mut o,
            "lgh_datagrams_total",
            "Semtech UDP datagrams seen on the tap, by type.",
            "counter",
        );
        for (i, name) in KIND_NAMES.iter().enumerate() {
            sample(
                &mut o,
                "lgh_datagrams_total",
                &[("type", name)],
                &g.datagrams[i].to_string(),
            );
        }
        fam(
            &mut o,
            "lgh_parse_errors_total",
            "Datagrams that could not be parsed (they are still relayed).",
            "counter",
        );
        for kind in ["too_short", "bad_version", "unknown_type", "bad_json"] {
            let v = g.parse_errors.get(kind).copied().unwrap_or(0);
            sample(
                &mut o,
                "lgh_parse_errors_total",
                &[("kind", kind)],
                &v.to_string(),
            );
        }
        fam(
            &mut o,
            "lgh_rxpk_parse_errors_total",
            "rxpk entries inside a valid PUSH_DATA that were skipped.",
            "counter",
        );
        sample(
            &mut o,
            "lgh_rxpk_parse_errors_total",
            &[],
            &g.rxpk_errors.to_string(),
        );
        fam(
            &mut o,
            "lgh_relay_errors_total",
            "UDP relay problems (send failures, evicted idle sessions).",
            "counter",
        );
        for kind in ["send_upstream", "send_downstream", "session_evicted"] {
            let v = g.relay_errors.get(kind).copied().unwrap_or(0);
            sample(
                &mut o,
                "lgh_relay_errors_total",
                &[("kind", kind)],
                &v.to_string(),
            );
        }

        // ---- uplink packets per channel / SF
        fam(&mut o, "lgh_rx_packets_total", "Uplink packets forwarded to the tap, by channel, spreading factor, bandwidth and CRC state.", "counter");
        for (key, s) in &g.rx {
            let (ch, sf, bw) = rx_labels(key);
            for crc in [Crc::Ok, Crc::Bad, Crc::None] {
                let v = s.crc[crc as usize];
                if v > 0 || crc == Crc::Ok {
                    sample(
                        &mut o,
                        "lgh_rx_packets_total",
                        &[
                            ("channel_mhz", &ch),
                            ("sf", &sf),
                            ("bw_khz", &bw),
                            ("crc", crc.label()),
                        ],
                        &v.to_string(),
                    );
                }
            }
        }
        fam(
            &mut o,
            "lgh_rx_airtime_seconds_total",
            "Estimated time on air of received uplinks (Semtech formula).",
            "counter",
        );
        for (key, s) in &g.rx {
            let (ch, sf, bw) = rx_labels(key);
            sample(
                &mut o,
                "lgh_rx_airtime_seconds_total",
                &[("channel_mhz", &ch), ("sf", &sf), ("bw_khz", &bw)],
                &fnum(s.airtime_s),
            );
        }
        hist_family(
            &mut o,
            "lgh_rx_rssi_dbm",
            "Uplink RSSI in dBm, by channel and spreading factor.",
            RSSI_BUCKETS,
            g.rx.iter().map(|(k, s)| (rx_labels(k), &s.rssi)),
        );
        hist_family(
            &mut o,
            "lgh_rx_snr_db",
            "Uplink SNR in dB (LoRa only), by channel and spreading factor.",
            SNR_BUCKETS,
            g.rx.iter()
                .filter(|(k, _)| k.1 != 0)
                .map(|(k, s)| (rx_labels(k), &s.snr)),
        );

        // ---- forwarder stat block
        fam(
            &mut o,
            "lgh_forwarder_rx_received_total",
            "Packets the concentrator received with a valid preamble (stat.rxnb, summed).",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_rx_received_total",
            &[],
            &g.stat_total.rxnb.to_string(),
        );
        fam(
            &mut o,
            "lgh_forwarder_rx_crc_ok_total",
            "Received packets with a valid CRC (stat.rxok, summed).",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_rx_crc_ok_total",
            &[],
            &g.stat_total.rxok.to_string(),
        );
        fam(
            &mut o,
            "lgh_forwarder_rx_forwarded_total",
            "Packets the forwarder sent upstream (stat.rxfw, summed).",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_rx_forwarded_total",
            &[],
            &g.stat_total.rxfw.to_string(),
        );
        fam(
            &mut o,
            "lgh_forwarder_downlink_received_total",
            "Downlink datagrams received from the server (stat.dwnb, summed).",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_downlink_received_total",
            &[],
            &g.stat_total.dwnb.to_string(),
        );
        fam(
            &mut o,
            "lgh_forwarder_downlink_emitted_total",
            "Packets emitted by the concentrator (stat.txnb, summed).",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_downlink_emitted_total",
            &[],
            &g.stat_total.txnb.to_string(),
        );
        fam(
            &mut o,
            "lgh_forwarder_stat_reports_total",
            "Forwarder stat reports received.",
            "counter",
        );
        sample(
            &mut o,
            "lgh_forwarder_stat_reports_total",
            &[],
            &g.stat_reports.to_string(),
        );
        fam(&mut o, "lgh_crc_error_ratio", "Share of received packets with a failed CRC over the last stat reports (1 - rxok/rxnb).", "gauge");
        sample(
            &mut o,
            "lgh_crc_error_ratio",
            &[],
            &fnum(g.crc_error_ratio()),
        );
        if let Some(a) = g.stat_last_ackr {
            fam(
                &mut o,
                "lgh_forwarder_upstream_ack_ratio",
                "Share of upstream datagrams the server acknowledged (stat.ackr, 0..1).",
                "gauge",
            );
            sample(
                &mut o,
                "lgh_forwarder_upstream_ack_ratio",
                &[],
                &fnum(a / 100.0),
            );
        }
        if let Some(t) = g.stat_last_unix {
            fam(
                &mut o,
                "lgh_forwarder_last_stat_timestamp_seconds",
                "Unix time of the last forwarder stat report.",
                "gauge",
            );
            sample(
                &mut o,
                "lgh_forwarder_last_stat_timestamp_seconds",
                &[],
                &fnum(t),
            );
        }

        // ---- downlink and duty cycle
        fam(
            &mut o,
            "lgh_tx_requests_total",
            "Downlink orders (PULL_RESP) passing the tap.",
            "counter",
        );
        sample(
            &mut o,
            "lgh_tx_requests_total",
            &[],
            &g.tx_requests.to_string(),
        );
        fam(
            &mut o,
            "lgh_tx_unparsed_total",
            "PULL_RESP datagrams whose txpk could not be understood.",
            "counter",
        );
        sample(
            &mut o,
            "lgh_tx_unparsed_total",
            &[],
            &g.tx_unparsed.to_string(),
        );
        fam(
            &mut o,
            "lgh_tx_ack_total",
            "TX_ACK results from the forwarder, by error string (NONE = accepted).",
            "counter",
        );
        if g.tx_ack.is_empty() {
            sample(&mut o, "lgh_tx_ack_total", &[("error", "NONE")], "0");
        }
        for (e, v) in &g.tx_ack {
            sample(&mut o, "lgh_tx_ack_total", &[("error", e)], &v.to_string());
        }
        fam(
            &mut o,
            "lgh_tx_airtime_seconds_total",
            "Estimated airtime of ordered downlinks.",
            "counter",
        );
        sample(
            &mut o,
            "lgh_tx_airtime_seconds_total",
            &[],
            &fnum(g.tx_airtime_s),
        );
        let bands = self.cfg.region.bands();
        let mut window = vec![0.0f64; bands.len()];
        for (_, a, b) in &g.duty {
            window[*b] += a;
        }
        fam(
            &mut o,
            "lgh_tx_airtime_window_seconds",
            "Downlink airtime in the last hour, per duty-cycle sub-band.",
            "gauge",
        );
        for (i, b) in bands.iter().enumerate() {
            sample(
                &mut o,
                "lgh_tx_airtime_window_seconds",
                &[("band", b.name)],
                &fnum(window[i]),
            );
        }
        fam(
            &mut o,
            "lgh_tx_duty_cycle_ratio",
            "Estimated downlink duty cycle over the last hour (airtime / 3600 s), per sub-band.",
            "gauge",
        );
        for (i, b) in bands.iter().enumerate() {
            sample(
                &mut o,
                "lgh_tx_duty_cycle_ratio",
                &[("band", b.name)],
                &fnum(window[i] / DUTY_WINDOW_S),
            );
        }
        fam(
            &mut o,
            "lgh_tx_duty_cycle_limit_ratio",
            "Regulatory duty-cycle limit of the sub-band (EU868 table), where one applies.",
            "gauge",
        );
        for b in bands.iter().filter(|b| b.limit > 0.0) {
            sample(
                &mut o,
                "lgh_tx_duty_cycle_limit_ratio",
                &[("band", b.name)],
                &fnum(b.limit),
            );
        }

        // ---- backhaul
        let age = |unix: Option<f64>| unix.map(|t| (now.unix - t).max(0.0));
        let fwd_up =
            age(g.forwarder_last_seen_unix).is_some_and(|a| a <= self.cfg.bridge_timeout_s);
        fam(
            &mut o,
            "lgh_forwarder_up",
            "1 if the packet forwarder sent a datagram recently.",
            "gauge",
        );
        sample(
            &mut o,
            "lgh_forwarder_up",
            &[],
            if fwd_up { "1" } else { "0" },
        );
        if let Some(t) = g.forwarder_last_seen_unix {
            fam(
                &mut o,
                "lgh_forwarder_last_seen_timestamp_seconds",
                "Unix time of the last PUSH_DATA or PULL_DATA from the forwarder.",
                "gauge",
            );
            sample(
                &mut o,
                "lgh_forwarder_last_seen_timestamp_seconds",
                &[],
                &fnum(t),
            );
        }
        let bridge_up = g
            .last_pull_ack_mono
            .is_some_and(|t| now.mono - t <= self.cfg.bridge_timeout_s);
        fam(
            &mut o,
            "lgh_bridge_up",
            "1 if the UDP bridge acknowledged a PULL_DATA recently (bridge process reachable).",
            "gauge",
        );
        sample(
            &mut o,
            "lgh_bridge_up",
            &[],
            if bridge_up { "1" } else { "0" },
        );
        fam(
            &mut o,
            "lgh_bridge_last_ack_timestamp_seconds",
            "Unix time of the last ack from the bridge, by ack type.",
            "gauge",
        );
        if let Some(t) = g.last_pull_ack_unix {
            sample(
                &mut o,
                "lgh_bridge_last_ack_timestamp_seconds",
                &[("type", "pull_ack")],
                &fnum(t),
            );
        }
        if let Some(t) = g.last_push_ack_unix {
            sample(
                &mut o,
                "lgh_bridge_last_ack_timestamp_seconds",
                &[("type", "push_ack")],
                &fnum(t),
            );
        }
        if let Some(m) = &g.mqtt {
            fam(&mut o, "lgh_mqtt_broker_reachable", "1 if a TCP connection to the configured MQTT broker succeeded at the last probe (not an MQTT-level check).", "gauge");
            sample(
                &mut o,
                "lgh_mqtt_broker_reachable",
                &[],
                if m.reachable { "1" } else { "0" },
            );
            fam(
                &mut o,
                "lgh_mqtt_broker_connect_seconds",
                "TCP connect time of the last probe.",
                "gauge",
            );
            sample(
                &mut o,
                "lgh_mqtt_broker_connect_seconds",
                &[],
                &fnum(m.connect_s),
            );
            fam(
                &mut o,
                "lgh_mqtt_broker_last_probe_timestamp_seconds",
                "Unix time of the last probe.",
                "gauge",
            );
            sample(
                &mut o,
                "lgh_mqtt_broker_last_probe_timestamp_seconds",
                &[],
                &fnum(m.checked_unix),
            );
        }

        // ---- host
        write_sys(&mut o, sys);
        fam(
            &mut o,
            "lgh_exporter_uptime_seconds",
            "Seconds since the exporter started.",
            "gauge",
        );
        sample(&mut o, "lgh_exporter_uptime_seconds", &[], &fnum(now.mono));
        o
    }
}

impl Inner {
    fn record_stat(&mut self, s: &Stat, now: Now) {
        self.stat_total.rxnb += s.rxnb;
        self.stat_total.rxok += s.rxok;
        self.stat_total.rxfw += s.rxfw;
        self.stat_total.dwnb += s.dwnb;
        self.stat_total.txnb += s.txnb;
        self.stat_reports += 1;
        self.stat_last_unix = Some(now.unix);
        if s.ackr.is_some() {
            self.stat_last_ackr = s.ackr;
        }
        self.stat_window.push_back((s.rxnb, s.rxok));
        while self.stat_window.len() > STAT_WINDOW {
            self.stat_window.pop_front();
        }
    }

    fn crc_error_ratio(&self) -> f64 {
        let (rxnb, rxok) = self
            .stat_window
            .iter()
            .fold((0u64, 0u64), |a, b| (a.0 + b.0, a.1 + b.1));
        if rxnb == 0 {
            0.0
        } else {
            (rxnb.saturating_sub(rxok)) as f64 / rxnb as f64
        }
    }

    fn record_rxpk(&mut self, p: &semtech::Rxpk) {
        let mut key: RxKey = match p.rate {
            DataRate::Lora { sf, bw_khz } => ((p.freq_mhz * 1000.0).round() as u32, sf, bw_khz),
            DataRate::Fsk { .. } => ((p.freq_mhz * 1000.0).round() as u32, 0, 0),
        };
        if !self.rx.contains_key(&key) && self.rx.len() >= MAX_RX_SERIES {
            key = (0, 0, 0);
        }
        let s = self.rx.entry(key).or_insert_with(RxSeries::new);
        s.crc[p.crc as usize] += 1;
        s.rssi.observe(RSSI_BUCKETS, p.rssi);
        if let Some(snr) = p.snr {
            s.snr.observe(SNR_BUCKETS, snr);
        }
        if let Some(a) = rxpk_airtime_s(p) {
            s.airtime_s += a;
        }
    }

    fn prune_duty(&mut self, now_mono: f64) {
        while let Some((t, _, _)) = self.duty.front() {
            if now_mono - *t > DUTY_WINDOW_S {
                self.duty.pop_front();
            } else {
                break;
            }
        }
    }
}

fn rx_labels(k: &RxKey) -> (String, String, String) {
    match *k {
        (0, 0, 0) => ("other".into(), "other".into(), "other".into()),
        (khz, 0, _) => (
            format!("{:.3}", f64::from(khz) / 1000.0),
            "FSK".into(),
            "0".into(),
        ),
        (khz, sf, bw) => (
            format!("{:.3}", f64::from(khz) / 1000.0),
            format!("SF{sf}"),
            bw.to_string(),
        ),
    }
}

fn write_sys(o: &mut String, s: &SysSnapshot) {
    if let Some(v) = s.uptime_s {
        fam(o, "lgh_uptime_seconds", "Host uptime.", "gauge");
        sample(o, "lgh_uptime_seconds", &[], &fnum(v));
    }
    if let Some(v) = s.cpu_busy_ratio {
        fam(
            o,
            "lgh_cpu_busy_ratio",
            "Share of CPU time not idle since the previous scrape (since boot on the first).",
            "gauge",
        );
        sample(o, "lgh_cpu_busy_ratio", &[], &fnum(v));
    }
    if let Some(v) = s.load1 {
        fam(o, "lgh_load1", "1-minute load average.", "gauge");
        sample(o, "lgh_load1", &[], &fnum(v));
    }
    if let Some(v) = s.mem_total {
        fam(o, "lgh_memory_total_bytes", "Total RAM.", "gauge");
        sample(o, "lgh_memory_total_bytes", &[], &v.to_string());
    }
    if let Some(v) = s.mem_available {
        fam(
            o,
            "lgh_memory_available_bytes",
            "RAM available without swapping (MemAvailable).",
            "gauge",
        );
        sample(o, "lgh_memory_available_bytes", &[], &v.to_string());
    }
    if !s.disks.is_empty() {
        fam(o, "lgh_disk_total_bytes", "Filesystem size.", "gauge");
        for d in &s.disks {
            sample(
                o,
                "lgh_disk_total_bytes",
                &[("path", &d.path)],
                &d.total.to_string(),
            );
        }
        fam(
            o,
            "lgh_disk_avail_bytes",
            "Filesystem space available to unprivileged users.",
            "gauge",
        );
        for d in &s.disks {
            sample(
                o,
                "lgh_disk_avail_bytes",
                &[("path", &d.path)],
                &d.avail.to_string(),
            );
        }
    }
    if !s.temps.is_empty() {
        fam(
            o,
            "lgh_temperature_celsius",
            "Temperature from thermal zones or the configured sensor file.",
            "gauge",
        );
        for (name, c) in &s.temps {
            sample(o, "lgh_temperature_celsius", &[("sensor", name)], &fnum(*c));
        }
    }
    if let Some(v) = s.exporter_rss {
        fam(
            o,
            "lgh_exporter_resident_memory_bytes",
            "Resident set size of the exporter process.",
            "gauge",
        );
        sample(o, "lgh_exporter_resident_memory_bytes", &[], &v.to_string());
    }
}

// ---- exposition helpers

/// Format a float for the exposition format. Non-finite values become NaN.
pub fn fnum(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "NaN".to_string()
    }
}

pub fn escape_label(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s
}

fn fam(o: &mut String, name: &str, help: &str, ty: &str) {
    let _ = writeln!(
        o,
        "# HELP {name} {}",
        help.replace('\\', "\\\\").replace('\n', "\\n")
    );
    let _ = writeln!(o, "# TYPE {name} {ty}");
}

fn sample(o: &mut String, name: &str, labels: &[(&str, &str)], value: &str) {
    o.push_str(name);
    if !labels.is_empty() {
        o.push('{');
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            let _ = write!(o, "{k}=\"{}\"", escape_label(v));
        }
        o.push('}');
    }
    o.push(' ');
    o.push_str(value);
    o.push('\n');
}

fn hist_family<'a>(
    o: &mut String,
    name: &str,
    help: &str,
    bounds: &[f64],
    series: impl Iterator<Item = ((String, String, String), &'a Hist)>,
) {
    fam(o, name, help, "histogram");
    for ((ch, sf, bw), h) in series {
        let mut cum = 0u64;
        for (i, b) in bounds.iter().enumerate() {
            cum += h.counts[i];
            sample(
                o,
                &format!("{name}_bucket"),
                &[
                    ("channel_mhz", &ch),
                    ("sf", &sf),
                    ("bw_khz", &bw),
                    ("le", &fnum(*b)),
                ],
                &cum.to_string(),
            );
        }
        cum += h.counts[bounds.len()];
        sample(
            o,
            &format!("{name}_bucket"),
            &[
                ("channel_mhz", &ch),
                ("sf", &sf),
                ("bw_khz", &bw),
                ("le", "+Inf"),
            ],
            &cum.to_string(),
        );
        sample(
            o,
            &format!("{name}_sum"),
            &[("channel_mhz", &ch), ("sf", &sf), ("bw_khz", &bw)],
            &fnum(h.sum),
        );
        sample(
            o,
            &format!("{name}_count"),
            &[("channel_mhz", &ch), ("sf", &sf), ("bw_khz", &bw)],
            &h.count.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semtech::{
        build_datagram, PULL_ACK, PULL_DATA, PULL_RESP, PUSH_ACK, PUSH_DATA, TX_ACK,
    };

    const EUI: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    fn now(t: f64) -> Now {
        Now {
            mono: t,
            unix: 1_800_000_000.0 + t,
        }
    }

    fn render(m: &Metrics, t: f64) -> String {
        m.render(now(t), &SysSnapshot::default())
    }

    /// Value of the first sample whose line starts with `prefix`.
    fn val(text: &str, prefix: &str) -> f64 {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting with {prefix}"))
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn push(m: &Metrics, json: &str, t: f64) {
        m.observe(&build_datagram(2, 1, PUSH_DATA, Some(&EUI), json), now(t));
    }

    #[test]
    fn stat_counters_accumulate() {
        let m = Metrics::new(Config::default());
        push(
            &m,
            r#"{"stat":{"rxnb":10,"rxok":8,"rxfw":8,"ackr":100.0,"dwnb":2,"txnb":1}}"#,
            1.0,
        );
        push(
            &m,
            r#"{"stat":{"rxnb":5,"rxok":5,"rxfw":5,"ackr":50.0,"dwnb":0,"txnb":0}}"#,
            31.0,
        );
        let t = render(&m, 40.0);
        assert_eq!(val(&t, "lgh_forwarder_rx_received_total"), 15.0);
        assert_eq!(val(&t, "lgh_forwarder_rx_crc_ok_total"), 13.0);
        assert_eq!(val(&t, "lgh_forwarder_rx_forwarded_total"), 13.0);
        assert_eq!(val(&t, "lgh_forwarder_downlink_received_total"), 2.0);
        assert_eq!(val(&t, "lgh_forwarder_downlink_emitted_total"), 1.0);
        assert_eq!(val(&t, "lgh_forwarder_stat_reports_total"), 2.0);
        assert_eq!(val(&t, "lgh_forwarder_upstream_ack_ratio"), 0.5);
    }

    #[test]
    fn crc_error_ratio_is_windowed() {
        let m = Metrics::new(Config::default());
        // 10 reports of 100 received / 90 ok -> 10 %
        for i in 0..10 {
            push(
                &m,
                r#"{"stat":{"rxnb":100,"rxok":90,"rxfw":90}}"#,
                f64::from(i),
            );
        }
        assert!((val(&render(&m, 20.0), "lgh_crc_error_ratio") - 0.1).abs() < 1e-12);
        // Ten clean reports push the bad ones out of the window.
        for i in 10..20 {
            push(
                &m,
                r#"{"stat":{"rxnb":100,"rxok":100,"rxfw":100}}"#,
                f64::from(i),
            );
        }
        assert_eq!(val(&render(&m, 30.0), "lgh_crc_error_ratio"), 0.0);
    }

    #[test]
    fn crc_error_ratio_zero_traffic() {
        let m = Metrics::new(Config::default());
        push(&m, r#"{"stat":{"rxnb":0,"rxok":0,"rxfw":0}}"#, 1.0);
        assert_eq!(val(&render(&m, 2.0), "lgh_crc_error_ratio"), 0.0);
    }

    #[test]
    fn rxpk_counts_by_channel_sf_and_crc() {
        let m = Metrics::new(Config::default());
        push(&m, include_str!("../tests/fixtures/rxpk_multi.json"), 1.0);
        push(&m, include_str!("../tests/fixtures/rxpk_multi.json"), 2.0);
        let t = render(&m, 3.0);
        assert_eq!(
            val(
                &t,
                r#"lgh_rx_packets_total{channel_mhz="868.500",sf="SF7",bw_khz="125",crc="ok"}"#
            ),
            2.0
        );
        assert_eq!(
            val(
                &t,
                r#"lgh_rx_packets_total{channel_mhz="868.100",sf="SF12",bw_khz="125",crc="ok"}"#
            ),
            2.0
        );
        assert_eq!(
            val(
                &t,
                r#"lgh_rx_packets_total{channel_mhz="868.300",sf="SF9",bw_khz="125",crc="bad"}"#
            ),
            2.0
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let m = Metrics::new(Config::default());
        push(&m, include_str!("../tests/fixtures/rxpk_multi.json"), 1.0);
        let t = render(&m, 2.0);
        // SF7 on 868.5 had RSSI -57: first bucket that holds it is le=-50
        let base = r#"lgh_rx_rssi_dbm_bucket{channel_mhz="868.500",sf="SF7",bw_khz="125""#;
        assert_eq!(val(&t, &format!(r#"{base},le="-60"}}"#)), 0.0);
        assert_eq!(val(&t, &format!(r#"{base},le="-50"}}"#)), 1.0);
        assert_eq!(val(&t, &format!(r#"{base},le="+Inf"}}"#)), 1.0);
        assert_eq!(
            val(&t, r#"lgh_rx_rssi_dbm_sum{channel_mhz="868.500""#),
            -57.0
        );
        assert_eq!(val(&t, r#"lgh_rx_snr_db_sum{channel_mhz="868.500""#), 10.0);
        assert_eq!(val(&t, r#"lgh_rx_snr_db_count{channel_mhz="868.500""#), 1.0);
    }

    #[test]
    fn fsk_has_rssi_but_no_snr_series() {
        let m = Metrics::new(Config::default());
        push(
            &m,
            r#"{"rxpk":[{"freq":868.8,"stat":1,"modu":"FSK","datr":50000,"rssi":-80,"size":20}]}"#,
            1.0,
        );
        let t = render(&m, 2.0);
        assert!(t.contains(r#"lgh_rx_rssi_dbm_count{channel_mhz="868.800",sf="FSK",bw_khz="0"} 1"#));
        assert!(!t.contains("lgh_rx_snr_db_count{channel_mhz=\"868.800\""));
    }

    #[test]
    fn rx_airtime_accumulates() {
        let m = Metrics::new(Config::default());
        // 23 B at SF7/125 = 61.696 ms (TTN: 61.7 ms); two packets
        let j = r#"{"rxpk":[{"freq":868.1,"stat":1,"modu":"LORA","datr":"SF7BW125","codr":"4/5","lsnr":5,"rssi":-90,"size":23}]}"#;
        push(&m, j, 1.0);
        push(&m, j, 2.0);
        let t = render(&m, 3.0);
        let v = val(
            &t,
            r#"lgh_rx_airtime_seconds_total{channel_mhz="868.100",sf="SF7""#,
        );
        assert!((v - 2.0 * 0.061696).abs() < 1e-9, "{v}");
    }

    #[test]
    fn series_cap_folds_into_other() {
        let m = Metrics::new(Config::default());
        for i in 0..(MAX_RX_SERIES + 10) {
            let j = format!(
                r#"{{"rxpk":[{{"freq":{},"stat":1,"modu":"LORA","datr":"SF7BW125","codr":"4/5","lsnr":5,"rssi":-90,"size":23}}]}}"#,
                800.0 + i as f64 * 0.1
            );
            push(&m, &j, 1.0);
        }
        let t = render(&m, 2.0);
        assert_eq!(val(&t, r#"lgh_rx_packets_total{channel_mhz="other""#), 10.0);
    }

    #[test]
    fn downlink_airtime_and_duty_cycle() {
        let m = Metrics::new(Config::default());
        let j = include_str!("../tests/fixtures/pull_resp.json");
        for i in 0..10 {
            m.observe(
                &build_datagram(2, 1, PULL_RESP, None, j),
                now(f64::from(i) * 10.0),
            );
        }
        let t = render(&m, 100.0);
        assert_eq!(val(&t, "lgh_tx_requests_total"), 10.0);
        // 869.525 MHz is sub-band g3; 10 x 246.784 ms
        let air = 10.0 * 0.246784;
        assert!((val(&t, r#"lgh_tx_airtime_window_seconds{band="g3"}"#) - air).abs() < 1e-9);
        assert!((val(&t, r#"lgh_tx_duty_cycle_ratio{band="g3"}"#) - air / 3600.0).abs() < 1e-12);
        assert_eq!(val(&t, r#"lgh_tx_duty_cycle_limit_ratio{band="g3"}"#), 0.1);
        assert_eq!(val(&t, r#"lgh_tx_duty_cycle_ratio{band="g1"}"#), 0.0);
        assert!((val(&t, "lgh_tx_airtime_seconds_total") - air).abs() < 1e-9);
    }

    #[test]
    fn duty_window_forgets_old_airtime_but_counter_keeps_it() {
        let m = Metrics::new(Config::default());
        let j = include_str!("../tests/fixtures/pull_resp.json");
        m.observe(&build_datagram(2, 1, PULL_RESP, None, j), now(0.0));
        let t = render(&m, 3601.0);
        assert_eq!(val(&t, r#"lgh_tx_airtime_window_seconds{band="g3"}"#), 0.0);
        assert!(val(&t, "lgh_tx_airtime_seconds_total") > 0.2);
    }

    #[test]
    fn region_none_has_single_band_without_limit() {
        let m = Metrics::new(Config {
            region: Region::None,
            ..Config::default()
        });
        let j = include_str!("../tests/fixtures/pull_resp.json");
        m.observe(&build_datagram(2, 1, PULL_RESP, None, j), now(0.0));
        let t = render(&m, 1.0);
        assert!(val(&t, r#"lgh_tx_duty_cycle_ratio{band="all"}"#) > 0.0);
        assert!(!t.contains("lgh_tx_duty_cycle_limit_ratio{"));
    }

    #[test]
    fn tx_ack_errors_are_counted() {
        let m = Metrics::new(Config::default());
        m.observe(&build_datagram(2, 1, TX_ACK, Some(&EUI), ""), now(1.0));
        m.observe(
            &build_datagram(
                2,
                1,
                TX_ACK,
                Some(&EUI),
                r#"{"txpk_ack":{"error":"TOO_LATE"}}"#,
            ),
            now(2.0),
        );
        m.observe(
            &build_datagram(
                2,
                1,
                TX_ACK,
                Some(&EUI),
                r#"{"txpk_ack":{"error":"TOO_LATE"}}"#,
            ),
            now(3.0),
        );
        let t = render(&m, 4.0);
        assert_eq!(val(&t, r#"lgh_tx_ack_total{error="NONE"}"#), 1.0);
        assert_eq!(val(&t, r#"lgh_tx_ack_total{error="TOO_LATE"}"#), 2.0);
    }

    #[test]
    fn bridge_up_follows_pull_ack() {
        let m = Metrics::new(Config {
            bridge_timeout_s: 30.0,
            ..Config::default()
        });
        assert_eq!(val(&render(&m, 1.0), "lgh_bridge_up"), 0.0);
        m.observe(&[2, 0, 1, PULL_ACK], now(10.0));
        assert_eq!(val(&render(&m, 20.0), "lgh_bridge_up"), 1.0);
        assert_eq!(val(&render(&m, 41.0), "lgh_bridge_up"), 0.0);
        m.observe(&[2, 0, 1, PUSH_ACK], now(50.0));
        // a PUSH_ACK alone does not prove the downlink path
        assert_eq!(val(&render(&m, 51.0), "lgh_bridge_up"), 0.0);
        assert!(
            render(&m, 51.0).contains(r#"lgh_bridge_last_ack_timestamp_seconds{type="push_ack"}"#)
        );
    }

    #[test]
    fn forwarder_last_seen_and_up() {
        let m = Metrics::new(Config {
            bridge_timeout_s: 30.0,
            ..Config::default()
        });
        assert_eq!(val(&render(&m, 1.0), "lgh_forwarder_up"), 0.0);
        m.observe(&build_datagram(2, 1, PULL_DATA, Some(&EUI), ""), now(5.0));
        let t = render(&m, 10.0);
        assert_eq!(val(&t, "lgh_forwarder_up"), 1.0);
        assert_eq!(
            val(&t, "lgh_forwarder_last_seen_timestamp_seconds"),
            1_800_000_005.0
        );
        assert!(t.contains(r#"lgh_gateway_info{gateway_eui="0102030405060708"} 1"#));
        assert_eq!(val(&render(&m, 100.0), "lgh_forwarder_up"), 0.0);
    }

    #[test]
    fn parse_errors_are_counted_and_do_not_panic() {
        let m = Metrics::new(Config::default());
        m.observe(&[], now(1.0));
        m.observe(&[9, 9, 9, 9], now(1.0));
        m.observe(&[2, 0, 0, 77], now(1.0));
        push(&m, "{oops", 1.0);
        let t = render(&m, 2.0);
        assert_eq!(val(&t, r#"lgh_parse_errors_total{kind="too_short"}"#), 1.0);
        assert_eq!(
            val(&t, r#"lgh_parse_errors_total{kind="bad_version"}"#),
            1.0
        );
        assert_eq!(
            val(&t, r#"lgh_parse_errors_total{kind="unknown_type"}"#),
            1.0
        );
        assert_eq!(val(&t, r#"lgh_parse_errors_total{kind="bad_json"}"#), 1.0);
    }

    #[test]
    fn bad_rxpk_entries_counted_good_ones_kept() {
        let m = Metrics::new(Config::default());
        push(&m, include_str!("../tests/fixtures/rxpk_one_bad.json"), 1.0);
        let t = render(&m, 2.0);
        assert_eq!(val(&t, "lgh_rxpk_parse_errors_total"), 2.0);
        assert_eq!(
            val(&t, r#"lgh_rx_packets_total{channel_mhz="868.100",sf="SF8""#),
            1.0
        );
    }

    #[test]
    fn mqtt_probe_exposed_only_when_configured() {
        let m = Metrics::new(Config::default());
        assert!(!render(&m, 1.0).contains("lgh_mqtt_broker_reachable"));
        m.set_mqtt_probe(true, 0.012, 1_800_000_000.0);
        let t = render(&m, 2.0);
        assert_eq!(val(&t, "lgh_mqtt_broker_reachable"), 1.0);
        assert_eq!(val(&t, "lgh_mqtt_broker_connect_seconds"), 0.012);
    }

    #[test]
    fn label_escaping() {
        assert_eq!(escape_label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape_label("x\ny"), "x\\ny");
    }

    #[test]
    fn fnum_handles_non_finite() {
        assert_eq!(fnum(f64::INFINITY), "NaN");
        assert_eq!(fnum(f64::NAN), "NaN");
        assert_eq!(fnum(1.5), "1.5");
        assert_eq!(fnum(-7.5), "-7.5");
    }

    #[test]
    fn non_finite_rssi_is_ignored_by_histogram() {
        let mut h = Hist::new(RSSI_BUCKETS);
        h.observe(RSSI_BUCKETS, f64::NAN);
        assert_eq!(h.count, 0);
        h.observe(RSSI_BUCKETS, -200.0);
        assert_eq!(h.counts[0], 1);
        h.observe(RSSI_BUCKETS, 10.0);
        assert_eq!(*h.counts.last().unwrap(), 1);
    }
}

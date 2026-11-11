//! End-to-end over real UDP/TCP sockets on loopback: forwarder -> tap -> bridge and back.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lgh::metrics::{Config, Metrics};
use lgh::relay::Relay;
use lgh::semtech::{build_datagram, PULL_DATA, PULL_RESP, PUSH_ACK, PUSH_DATA};
use lgh::sys::SysCollector;
use lgh::{http, Clock};

const EUI: [u8; 8] = [0xaa, 0x55, 0x5a, 0, 0, 0, 1, 1];

fn udp() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s
}

struct Rig {
    tap: SocketAddr,
    bridge: UdpSocket,
    metrics: Arc<Metrics>,
    http: SocketAddr,
}

fn rig() -> Rig {
    let bridge = udp();
    let metrics = Arc::new(Metrics::new(Config::default()));
    let clock = Clock::new();
    let relay = Relay::bind(
        "127.0.0.1:0".parse().unwrap(),
        bridge.local_addr().unwrap(),
        metrics.clone(),
        clock,
    )
    .unwrap();
    let tap = relay.local_addr().unwrap();
    thread::spawn(move || {
        let _ = relay.run();
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let http_addr = listener.local_addr().unwrap();
    let m = metrics.clone();
    let sys = Arc::new(SysCollector::new(vec!["/".into()], vec![]));
    let render: Arc<http::RenderFn> = Arc::new(move || m.render(clock.now(), &sys.collect()));
    thread::spawn(move || {
        let _ = http::serve(listener, render);
    });
    Rig {
        tap,
        bridge,
        metrics,
        http: http_addr,
    }
}

fn get(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out)
        .unwrap_or_else(|e| panic!("{:?}: {e}", &request[..request.len().min(60)]));
    out
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("condition not met in time");
}

#[test]
fn uplink_is_forwarded_unchanged_and_counted() {
    let r = rig();
    let fwd = udp();
    let body = include_str!("fixtures/rxpk_multi.json");
    let dgram = build_datagram(2, 0x1234, PUSH_DATA, Some(&EUI), body);
    fwd.send_to(&dgram, r.tap).unwrap();

    let mut buf = [0u8; 4096];
    let (n, from) = r.bridge.recv_from(&mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        &dgram[..],
        "datagram must pass through byte for byte"
    );

    // PUSH_ACK goes back to the forwarder through the tap.
    r.bridge.send_to(&[2, 0x12, 0x34, PUSH_ACK], from).unwrap();
    let (n, _) = fwd.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], &[2, 0x12, 0x34, PUSH_ACK]);

    wait_for(|| {
        let text = r.metrics.render(Clock::new().now(), &Default::default());
        text.contains(r#"lgh_datagrams_total{type="push_ack"} 1"#)
            && text.contains(
                r#"lgh_rx_packets_total{channel_mhz="868.500",sf="SF7",bw_khz="125",crc="ok"} 1"#,
            )
    });
}

#[test]
fn downlink_order_reaches_the_forwarder_and_is_counted() {
    let r = rig();
    let fwd = udp();
    fwd.send_to(&build_datagram(2, 1, PULL_DATA, Some(&EUI), ""), r.tap)
        .unwrap();
    let mut buf = [0u8; 4096];
    let (_, from) = r.bridge.recv_from(&mut buf).unwrap();

    let order = build_datagram(
        2,
        7,
        PULL_RESP,
        None,
        include_str!("fixtures/pull_resp.json"),
    );
    r.bridge.send_to(&order, from).unwrap();
    let (n, _) = fwd.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], &order[..]);

    wait_for(|| {
        let text = r.metrics.render(Clock::new().now(), &Default::default());
        text.contains("lgh_tx_requests_total 1")
            && text.contains(r#"lgh_tx_duty_cycle_ratio{band="g3"} 0.0000"#)
    });
}

#[test]
fn garbage_is_relayed_and_counted_not_fatal() {
    let r = rig();
    let fwd = udp();
    let junk = [0x99u8, 1, 2, 3, 4, 5];
    fwd.send_to(&junk, r.tap).unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = r.bridge.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], &junk);
    // The relay still works afterwards.
    let ok = build_datagram(
        2,
        2,
        PUSH_DATA,
        Some(&EUI),
        r#"{"stat":{"rxnb":1,"rxok":1,"rxfw":1}}"#,
    );
    fwd.send_to(&ok, r.tap).unwrap();
    let (n, _) = r.bridge.recv_from(&mut buf).unwrap_or((0, r.tap));
    assert!(n > 0);
    wait_for(|| {
        let t = r.metrics.render(Clock::new().now(), &Default::default());
        t.contains(r#"lgh_parse_errors_total{kind="bad_version"} 1"#)
            && t.contains("lgh_forwarder_stat_reports_total 1")
    });
}

#[test]
fn two_forwarder_sockets_get_separate_sessions() {
    let r = rig();
    let (up, down) = (udp(), udp());
    up.send_to(&build_datagram(2, 1, PUSH_DATA, Some(&EUI), "{}"), r.tap)
        .unwrap();
    down.send_to(&build_datagram(2, 2, PULL_DATA, Some(&EUI), ""), r.tap)
        .unwrap();
    let mut buf = [0u8; 256];
    let (_, a) = r.bridge.recv_from(&mut buf).unwrap();
    let (_, b) = r.bridge.recv_from(&mut buf).unwrap();
    assert_ne!(a, b, "bridge should see two distinct source ports");
}

#[test]
fn forwarder_restarting_on_new_ports_is_never_locked_out() {
    let r = rig();
    let mut buf = [0u8; 256];
    // Many more distinct source ports than the session table holds.
    for i in 0..(lgh::relay::MAX_SESSIONS * 2 + 3) {
        let fwd = udp();
        let d = build_datagram(2, i as u16, PUSH_DATA, Some(&EUI), "{}");
        fwd.send_to(&d, r.tap).unwrap();
        let (n, from) = r.bridge.recv_from(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            &d[..],
            "datagram {i} must still reach the bridge"
        );
        // and the reply still finds its way back to the newest socket
        r.bridge.send_to(&[2, 0, 1, PUSH_ACK], from).unwrap();
        let (n, _) = fwd.recv_from(&mut buf).unwrap();
        assert_eq!(n, 4);
    }
    wait_for(|| {
        r.metrics
            .render(Clock::new().now(), &Default::default())
            .contains(r#"lgh_relay_errors_total{kind="session_evicted"} 19"#)
    });
}

#[test]
fn http_endpoints() {
    let r = rig();
    let ok = get(r.http, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(ok.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8"));
    assert!(ok.contains("# TYPE lgh_bridge_up gauge"));
    // Content-Length matches the body
    let (head, body) = ok.split_once("\r\n\r\n").unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(len, body.len());

    assert!(get(r.http, "GET /healthz HTTP/1.1\r\n\r\n").ends_with("ok\n"));
    assert!(get(r.http, "GET /nope HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 404"));
    assert!(get(r.http, "POST /metrics HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 405"));
    let head = get(r.http, "HEAD /metrics HTTP/1.1\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200") && head.ends_with("\r\n\r\n"));
    // query strings are ignored
    assert!(get(r.http, "GET /metrics?x=1 HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    // oversized head is refused instead of buffered forever
    let big = format!("GET /metrics HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(20_000));
    // (the server may reset the connection before the client reads the reply; both are a refusal)
    let mut c = TcpStream::connect(r.http).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let _ = c.write_all(big.as_bytes());
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    assert!(
        out.is_empty() || out.starts_with("HTTP/1.1 431"),
        "refused with {:?}",
        &out[..out.len().min(40)]
    );
}

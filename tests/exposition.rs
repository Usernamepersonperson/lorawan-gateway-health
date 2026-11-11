//! Checks that the rendered text is valid Prometheus exposition (format 0.0.4).

use std::collections::{BTreeMap, BTreeSet};

use lgh::metrics::{Config, Metrics, Now};
use lgh::semtech::{build_datagram, PULL_ACK, PULL_DATA, PULL_RESP, PUSH_ACK, PUSH_DATA, TX_ACK};
use lgh::sys::{Disk, SysSnapshot};

const EUI: [u8; 8] = [0xaa, 0x55, 0x5a, 0, 0, 0, 1, 1];

fn now(t: f64) -> Now {
    Now {
        mono: t,
        unix: 1_800_000_000.0 + t,
    }
}

fn loaded() -> (Metrics, String) {
    let m = Metrics::new(Config::default());
    for (i, body) in [
        include_str!("fixtures/rxpk_multi.json"),
        include_str!("fixtures/rxpk_one_bad.json"),
        r#"{"stat":{"time":"2026-11-08 10:00:00 GMT","rxnb":12,"rxok":10,"rxfw":10,"ackr":100.0,"dwnb":1,"txnb":1}}"#,
    ]
    .iter()
    .enumerate()
    {
        m.observe(&build_datagram(2, i as u16, PUSH_DATA, Some(&EUI), body), now(i as f64));
    }
    m.observe(&build_datagram(2, 1, PULL_DATA, Some(&EUI), ""), now(3.0));
    m.observe(&[2, 0, 1, PUSH_ACK], now(3.0));
    m.observe(&[2, 0, 1, PULL_ACK], now(3.0));
    m.observe(
        &build_datagram(
            2,
            1,
            PULL_RESP,
            None,
            include_str!("fixtures/pull_resp.json"),
        ),
        now(4.0),
    );
    m.observe(&build_datagram(2, 1, TX_ACK, Some(&EUI), ""), now(5.0));
    m.observe(&[0xff, 0xff], now(5.0));
    m.set_mqtt_probe(true, 0.004, 1_800_000_005.0);
    let sys = SysSnapshot {
        uptime_s: Some(1234.5),
        load1: Some(0.25),
        mem_total: Some(512 << 20),
        mem_available: Some(300 << 20),
        cpu_busy_ratio: Some(0.12),
        disks: vec![
            Disk {
                path: "/".into(),
                total: 1 << 30,
                avail: 1 << 29,
            },
            Disk {
                path: "/data\"x".into(),
                total: 5,
                avail: 4,
            },
        ],
        temps: vec![("thermal_zone0:cpu".into(), 48.5)],
        exporter_rss: Some(2 << 20),
    };
    let text = m.render(now(10.0), &sys);
    (m, text)
}

fn is_metric_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_' || ch == ':')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':')
}

/// Split `name{a="b",c="d"} value` into parts, honouring escapes inside label values.
fn split_sample(line: &str) -> (String, Vec<(String, String)>, String) {
    let (head, value) = line.rsplit_once(' ').expect("sample has a value");
    match head.find('{') {
        None => (head.to_string(), vec![], value.to_string()),
        Some(i) => {
            assert!(head.ends_with('}'), "unterminated labels: {line}");
            let name = head[..i].to_string();
            let inner = &head[i + 1..head.len() - 1];
            let mut labels = Vec::new();
            let mut chars = inner.chars().peekable();
            while chars.peek().is_some() {
                let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
                assert_eq!(
                    chars.next(),
                    Some('"'),
                    "label value must be quoted: {line}"
                );
                let mut val = String::new();
                loop {
                    match chars.next().expect("unterminated label value") {
                        '\\' => match chars.next().expect("dangling escape") {
                            'n' => val.push('\n'),
                            c @ ('\\' | '"') => val.push(c),
                            c => panic!("bad escape \\{c} in {line}"),
                        },
                        '"' => break,
                        c => val.push(c),
                    }
                }
                labels.push((key, val));
                if chars.peek() == Some(&',') {
                    chars.next();
                }
            }
            (name, labels, value.to_string())
        }
    }
}

#[test]
fn every_line_is_comment_or_valid_sample() {
    let (_, text) = loaded();
    assert!(text.ends_with('\n'));
    for line in text.lines() {
        assert!(!line.is_empty(), "blank line inside exposition");
        if let Some(rest) = line.strip_prefix("# ") {
            assert!(
                rest.starts_with("HELP ") || rest.starts_with("TYPE "),
                "{line}"
            );
            continue;
        }
        let (name, labels, value) = split_sample(line);
        assert!(is_metric_name(&name), "bad metric name in {line}");
        for (k, _) in &labels {
            assert!(is_metric_name(k) && !k.contains(':'), "bad label name {k}");
        }
        assert!(
            value == "NaN" || value == "+Inf" || value == "-Inf" || value.parse::<f64>().is_ok(),
            "bad value in {line}"
        );
    }
}

#[test]
fn help_and_type_precede_samples_once_per_family() {
    let (_, text) = loaded();
    let mut typed: BTreeMap<String, String> = BTreeMap::new();
    let mut helped = BTreeSet::new();
    let mut closed = BTreeSet::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(r) = line.strip_prefix("# HELP ") {
            let name = r.split(' ').next().unwrap().to_string();
            assert!(helped.insert(name.clone()), "duplicate HELP for {name}");
            if let Some(prev) = current.replace(name) {
                closed.insert(prev);
            }
        } else if let Some(r) = line.strip_prefix("# TYPE ") {
            let mut it = r.split(' ');
            let (name, ty) = (it.next().unwrap(), it.next().unwrap());
            assert!(["counter", "gauge", "histogram"].contains(&ty), "{line}");
            assert_eq!(current.as_deref(), Some(name), "TYPE must follow its HELP");
            assert!(typed.insert(name.to_string(), ty.to_string()).is_none());
        } else {
            let (name, _, _) = split_sample(line);
            let base = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| {
                    name.strip_suffix(s)
                        .filter(|b| typed.get(*b).map(String::as_str) == Some("histogram"))
                })
                .unwrap_or(&name);
            assert_eq!(
                current.as_deref(),
                Some(base),
                "sample {name} is not inside its family block"
            );
            assert!(!closed.contains(base), "family {base} is not contiguous");
        }
    }
    assert!(
        typed.len() > 30,
        "expected many families, got {}",
        typed.len()
    );
}

#[test]
fn counters_end_in_total() {
    let (_, text) = loaded();
    for line in text.lines().filter_map(|l| l.strip_prefix("# TYPE ")) {
        let mut it = line.split(' ');
        let (name, ty) = (it.next().unwrap(), it.next().unwrap());
        if ty == "counter" {
            assert!(name.ends_with("_total"), "{name}");
        }
    }
}

#[test]
fn histograms_are_consistent() {
    let (_, text) = loaded();
    // series key (labels without le) -> [(le, count)]
    let mut buckets: BTreeMap<(String, String), Vec<(f64, f64)>> = BTreeMap::new();
    let mut counts: BTreeMap<(String, String), f64> = BTreeMap::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (name, labels, value) = split_sample(line);
        let v: f64 = value.parse().unwrap();
        let key_labels = |skip_le: bool| {
            labels
                .iter()
                .filter(|(k, _)| !(skip_le && k == "le"))
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        if let Some(base) = name.strip_suffix("_bucket") {
            let le = labels
                .iter()
                .find(|(k, _)| k == "le")
                .expect("bucket without le")
                .1
                .clone();
            let le_v = if le == "+Inf" {
                f64::INFINITY
            } else {
                le.parse().unwrap()
            };
            buckets
                .entry((base.to_string(), key_labels(true)))
                .or_default()
                .push((le_v, v));
        } else if let Some(base) = name.strip_suffix("_count") {
            counts.insert((base.to_string(), key_labels(false)), v);
        }
    }
    assert!(!buckets.is_empty());
    for (key, series) in &buckets {
        assert!(
            series.windows(2).all(|w| w[0].0 < w[1].0),
            "le not increasing for {key:?}"
        );
        assert!(
            series.windows(2).all(|w| w[0].1 <= w[1].1),
            "buckets not cumulative for {key:?}"
        );
        let (inf_le, inf_v) = series.last().unwrap();
        assert!(inf_le.is_infinite());
        assert_eq!(
            counts.get(key),
            Some(inf_v),
            "+Inf bucket must equal _count for {key:?}"
        );
    }
}

#[test]
fn awkward_label_values_round_trip() {
    let (_, text) = loaded();
    let line = text
        .lines()
        .find(|l| l.starts_with("lgh_disk_total_bytes{path=\"/data"))
        .unwrap();
    let (_, labels, _) = split_sample(line);
    assert_eq!(labels[0].1, "/data\"x");
}

#[test]
fn expected_metric_families_exist() {
    let (_, text) = loaded();
    for name in [
        "lgh_rx_packets_total",
        "lgh_rx_rssi_dbm",
        "lgh_rx_snr_db",
        "lgh_rx_airtime_seconds_total",
        "lgh_crc_error_ratio",
        "lgh_tx_duty_cycle_ratio",
        "lgh_tx_duty_cycle_limit_ratio",
        "lgh_bridge_up",
        "lgh_forwarder_up",
        "lgh_mqtt_broker_reachable",
        "lgh_temperature_celsius",
        "lgh_cpu_busy_ratio",
        "lgh_memory_available_bytes",
        "lgh_uptime_seconds",
        "lgh_disk_avail_bytes",
    ] {
        assert!(text.contains(&format!("# TYPE {name} ")), "missing {name}");
    }
}

#[test]
fn empty_state_still_renders_valid_text() {
    let m = Metrics::new(Config::default());
    let text = m.render(now(0.0), &SysSnapshot::default());
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (name, _, value) = split_sample(line);
        assert!(is_metric_name(&name));
        assert!(value.parse::<f64>().is_ok(), "{line}");
    }
    assert!(text.contains("lgh_bridge_up 0"));
}

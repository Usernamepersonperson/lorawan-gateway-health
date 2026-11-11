//! Parser for the Semtech UDP packet-forwarder protocol (PROTOCOL.md, version 1/2).
//!
//! Datagram layout: `version(1) token(2) type(1) [gateway EUI(8)] [JSON]`.

use serde::Deserialize;
use serde_json::Value;

/// Datagram identifiers.
pub const PUSH_DATA: u8 = 0x00;
pub const PUSH_ACK: u8 = 0x01;
pub const PULL_DATA: u8 = 0x02;
pub const PULL_RESP: u8 = 0x03;
pub const PULL_ACK: u8 = 0x04;
pub const TX_ACK: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    PushData,
    PushAck,
    PullData,
    PullResp,
    PullAck,
    TxAck,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::PushData => "push_data",
            Kind::PushAck => "push_ack",
            Kind::PullData => "pull_data",
            Kind::PullResp => "pull_resp",
            Kind::PullAck => "pull_ack",
            Kind::TxAck => "tx_ack",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    TooShort,
    BadVersion(u8),
    UnknownType(u8),
    BadJson,
}

impl ParseError {
    /// Stable label value for the `lgh_parse_errors_total` metric.
    pub fn label(&self) -> &'static str {
        match self {
            ParseError::TooShort => "too_short",
            ParseError::BadVersion(_) => "bad_version",
            ParseError::UnknownType(_) => "unknown_type",
            ParseError::BadJson => "bad_json",
        }
    }
}

/// CRC state reported by the concentrator for one received packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Crc {
    Ok,
    Bad,
    None,
}

impl Crc {
    pub fn label(self) -> &'static str {
        match self {
            Crc::Ok => "ok",
            Crc::Bad => "bad",
            Crc::None => "none",
        }
    }
}

/// Modulation of a packet: LoRa with SF/BW, or FSK with a bit rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataRate {
    Lora { sf: u8, bw_khz: u32 },
    Fsk { bps: u32 },
}

/// One received packet (`rxpk` entry), reduced to the fields the exporter needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Rxpk {
    pub freq_mhz: f64,
    pub crc: Crc,
    pub rate: DataRate,
    /// Coding rate numerator offset: 1 for "4/5" ... 4 for "4/8".
    pub cr: u8,
    pub rssi: f64,
    pub snr: Option<f64>,
    pub size: u32,
}

/// A downlink order (`txpk` in PULL_RESP).
#[derive(Debug, Clone, PartialEq)]
pub struct Txpk {
    pub freq_mhz: f64,
    pub rate: DataRate,
    pub cr: u8,
    pub size: u32,
    pub preamble: u32,
    pub no_crc: bool,
    pub imme: bool,
}

/// The forwarder's periodic status block. Values count since the previous report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    pub rxnb: u64,
    pub rxok: u64,
    pub rxfw: u64,
    pub ackr: Option<f64>,
    pub dwnb: u64,
    pub txnb: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    PushData {
        eui: [u8; 8],
        stat: Option<Stat>,
        rxpk: Vec<Rxpk>,
        /// rxpk entries that could not be understood (still forwarded unchanged by the relay).
        rxpk_errors: usize,
    },
    PushAck,
    PullData {
        eui: [u8; 8],
    },
    PullResp {
        txpk: Option<Txpk>,
    },
    PullAck,
    TxAck {
        eui: [u8; 8],
        /// "NONE" when the forwarder sent no JSON (success), else the `error` string.
        error: String,
    },
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::PushData { .. } => Kind::PushData,
            Message::PushAck => Kind::PushAck,
            Message::PullData { .. } => Kind::PullData,
            Message::PullResp { .. } => Kind::PullResp,
            Message::PullAck => Kind::PullAck,
            Message::TxAck { .. } => Kind::TxAck,
        }
    }
}

/// Parse "SF7BW125" -> (7, 125). Rejects anything else.
pub fn parse_lora_datr(s: &str) -> Option<(u8, u32)> {
    let rest = s.strip_prefix("SF")?;
    let (sf, bw) = rest.split_once("BW")?;
    let sf: u8 = sf.parse().ok()?;
    let bw: u32 = bw.parse().ok()?;
    if !(5..=12).contains(&sf) || bw == 0 {
        return None;
    }
    Some((sf, bw))
}

/// Parse "4/5" .. "4/8" -> 1..4.
pub fn parse_codr(s: &str) -> Option<u8> {
    match s {
        "4/5" => Some(1),
        "4/6" => Some(2),
        "4/7" => Some(3),
        "4/8" => Some(4),
        _ => None,
    }
}

fn parse_rate(modu: &str, datr: &Value) -> Option<DataRate> {
    match (modu, datr) {
        ("LORA", Value::String(s)) => {
            let (sf, bw_khz) = parse_lora_datr(s)?;
            Some(DataRate::Lora { sf, bw_khz })
        }
        ("FSK", v) => {
            let bps = v.as_u64()?;
            Some(DataRate::Fsk { bps: bps as u32 })
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawRxpk {
    freq: f64,
    #[serde(default)]
    stat: Option<i64>,
    modu: String,
    datr: Value,
    #[serde(default)]
    codr: Option<String>,
    rssi: f64,
    #[serde(default)]
    lsnr: Option<f64>,
    size: u32,
}

impl RawRxpk {
    fn convert(self) -> Option<Rxpk> {
        let rate = parse_rate(&self.modu, &self.datr)?;
        let crc = match self.stat {
            Some(1) => Crc::Ok,
            Some(-1) => Crc::Bad,
            _ => Crc::None,
        };
        // FSK carries no coding rate; default to 1 so airtime code never sees 0.
        let cr = self.codr.as_deref().and_then(parse_codr).unwrap_or(1);
        Some(Rxpk {
            freq_mhz: self.freq,
            crc,
            rate,
            cr,
            rssi: self.rssi,
            snr: self.lsnr,
            size: self.size,
        })
    }
}

#[derive(Deserialize)]
struct RawStat {
    #[serde(default)]
    rxnb: u64,
    #[serde(default)]
    rxok: u64,
    #[serde(default)]
    rxfw: u64,
    #[serde(default)]
    ackr: Option<f64>,
    #[serde(default)]
    dwnb: u64,
    #[serde(default)]
    txnb: u64,
}

#[derive(Deserialize)]
struct RawTxpk {
    freq: f64,
    modu: String,
    datr: Value,
    #[serde(default)]
    codr: Option<String>,
    size: u32,
    #[serde(default)]
    prea: Option<u32>,
    #[serde(default)]
    ncrc: Option<bool>,
    #[serde(default)]
    imme: Option<bool>,
}

fn eui_at(buf: &[u8]) -> Result<[u8; 8], ParseError> {
    buf.get(4..12)
        .map(|s| {
            let mut e = [0u8; 8];
            e.copy_from_slice(s);
            e
        })
        .ok_or(ParseError::TooShort)
}

/// Hex-encode a gateway EUI (lower case, 16 chars).
pub fn eui_hex(eui: &[u8; 8]) -> String {
    eui.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse one UDP datagram.
pub fn parse_datagram(buf: &[u8]) -> Result<Message, ParseError> {
    if buf.len() < 4 {
        return Err(ParseError::TooShort);
    }
    let version = buf[0];
    if version != 1 && version != 2 {
        return Err(ParseError::BadVersion(version));
    }
    match buf[3] {
        PUSH_DATA => {
            let eui = eui_at(buf)?;
            let json = &buf[12..];
            let v: Value = serde_json::from_slice(json).map_err(|_| ParseError::BadJson)?;
            let obj = v.as_object().ok_or(ParseError::BadJson)?;
            let stat = match obj.get("stat") {
                Some(s) if s.is_object() => {
                    let raw: RawStat =
                        serde_json::from_value(s.clone()).map_err(|_| ParseError::BadJson)?;
                    Some(Stat {
                        rxnb: raw.rxnb,
                        rxok: raw.rxok,
                        rxfw: raw.rxfw,
                        ackr: raw.ackr,
                        dwnb: raw.dwnb,
                        txnb: raw.txnb,
                    })
                }
                Some(_) => return Err(ParseError::BadJson),
                None => None,
            };
            let mut rxpk = Vec::new();
            let mut rxpk_errors = 0;
            match obj.get("rxpk") {
                Some(Value::Array(items)) => {
                    for it in items {
                        match serde_json::from_value::<RawRxpk>(it.clone())
                            .ok()
                            .and_then(RawRxpk::convert)
                        {
                            Some(p) => rxpk.push(p),
                            None => rxpk_errors += 1,
                        }
                    }
                }
                Some(_) => return Err(ParseError::BadJson),
                None => {}
            }
            Ok(Message::PushData {
                eui,
                stat,
                rxpk,
                rxpk_errors,
            })
        }
        PUSH_ACK => Ok(Message::PushAck),
        PULL_DATA => Ok(Message::PullData { eui: eui_at(buf)? }),
        PULL_RESP => {
            let v: Value = serde_json::from_slice(&buf[4..]).map_err(|_| ParseError::BadJson)?;
            let txpk = v
                .get("txpk")
                .and_then(|t| serde_json::from_value::<RawTxpk>(t.clone()).ok())
                .and_then(|t| {
                    let rate = parse_rate(&t.modu, &t.datr)?;
                    Some(Txpk {
                        freq_mhz: t.freq,
                        rate,
                        cr: t.codr.as_deref().and_then(parse_codr).unwrap_or(1),
                        size: t.size,
                        preamble: t.prea.unwrap_or(8),
                        // PROTOCOL.TXT marks ncrc optional; absent means the PHY CRC stays on.
                        no_crc: t.ncrc.unwrap_or(false),
                        imme: t.imme.unwrap_or(false),
                    })
                });
            Ok(Message::PullResp { txpk })
        }
        PULL_ACK => Ok(Message::PullAck),
        TX_ACK => {
            let eui = eui_at(buf)?;
            let json = &buf[12..];
            let error = if json.is_empty() {
                "NONE".to_string()
            } else {
                let v: Value = serde_json::from_slice(json).map_err(|_| ParseError::BadJson)?;
                v.get("txpk_ack")
                    .and_then(|a| a.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("NONE")
                    .to_string()
            };
            Ok(Message::TxAck { eui, error })
        }
        other => Err(ParseError::UnknownType(other)),
    }
}

/// Build a datagram (used by the replay tool and the tests).
pub fn build_datagram(
    version: u8,
    token: u16,
    kind: u8,
    eui: Option<&[u8; 8]>,
    json: &str,
) -> Vec<u8> {
    let mut v = vec![version, (token >> 8) as u8, token as u8, kind];
    if let Some(e) = eui {
        v.extend_from_slice(e);
    }
    v.extend_from_slice(json.as_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const EUI: [u8; 8] = [0xaa, 0x55, 0x5a, 0, 0, 0, 1, 2];

    #[test]
    fn datr_parsing() {
        assert_eq!(parse_lora_datr("SF7BW125"), Some((7, 125)));
        assert_eq!(parse_lora_datr("SF12BW500"), Some((12, 500)));
        assert_eq!(parse_lora_datr("SF13BW125"), None);
        assert_eq!(parse_lora_datr("SF7"), None);
        assert_eq!(parse_lora_datr("SFxBW125"), None);
        assert_eq!(parse_lora_datr(""), None);
    }

    #[test]
    fn codr_parsing() {
        assert_eq!(parse_codr("4/5"), Some(1));
        assert_eq!(parse_codr("4/8"), Some(4));
        assert_eq!(parse_codr("4/9"), None);
    }

    #[test]
    fn short_and_bad_headers() {
        assert_eq!(parse_datagram(&[]), Err(ParseError::TooShort));
        assert_eq!(parse_datagram(&[2, 0, 1]), Err(ParseError::TooShort));
        assert_eq!(
            parse_datagram(&[3, 0, 1, 0]),
            Err(ParseError::BadVersion(3))
        );
        assert_eq!(
            parse_datagram(&[2, 0, 1, 9]),
            Err(ParseError::UnknownType(9))
        );
    }

    #[test]
    fn acks() {
        assert_eq!(parse_datagram(&[2, 1, 2, PUSH_ACK]), Ok(Message::PushAck));
        assert_eq!(parse_datagram(&[1, 1, 2, PULL_ACK]), Ok(Message::PullAck));
    }

    #[test]
    fn pull_data_needs_eui() {
        assert_eq!(
            parse_datagram(&[2, 0, 0, PULL_DATA, 1, 2]),
            Err(ParseError::TooShort)
        );
        let d = build_datagram(2, 7, PULL_DATA, Some(&EUI), "");
        assert_eq!(parse_datagram(&d), Ok(Message::PullData { eui: EUI }));
    }

    #[test]
    fn push_data_stat_only() {
        let j = r#"{"stat":{"time":"2026-11-08 10:00:00 GMT","rxnb":10,"rxok":9,"rxfw":9,"ackr":100.0,"dwnb":1,"txnb":1}}"#;
        let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
        match parse_datagram(&d).unwrap() {
            Message::PushData {
                eui,
                stat,
                rxpk,
                rxpk_errors,
            } => {
                assert_eq!(eui, EUI);
                assert!(rxpk.is_empty());
                assert_eq!(rxpk_errors, 0);
                let s = stat.unwrap();
                assert_eq!((s.rxnb, s.rxok, s.rxfw, s.dwnb, s.txnb), (10, 9, 9, 1, 1));
                assert_eq!(s.ackr, Some(100.0));
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn push_data_rxpk_fixture() {
        let j = include_str!("../tests/fixtures/rxpk_multi.json");
        let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
        match parse_datagram(&d).unwrap() {
            Message::PushData {
                rxpk, rxpk_errors, ..
            } => {
                assert_eq!(rxpk.len(), 3);
                assert_eq!(rxpk_errors, 0);
                assert_eq!(rxpk[0].rate, DataRate::Lora { sf: 7, bw_khz: 125 });
                assert_eq!(rxpk[0].crc, Crc::Ok);
                assert_eq!(rxpk[0].cr, 1);
                assert_eq!(rxpk[0].size, 23);
                assert!((rxpk[0].rssi - -57.0).abs() < 1e-9);
                assert_eq!(rxpk[0].snr, Some(10.0));
                assert_eq!(
                    rxpk[1].rate,
                    DataRate::Lora {
                        sf: 12,
                        bw_khz: 125
                    }
                );
                assert_eq!(rxpk[2].crc, Crc::Bad);
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn fsk_rxpk() {
        let j =
            r#"{"rxpk":[{"freq":868.8,"stat":1,"modu":"FSK","datr":50000,"rssi":-80,"size":20}]}"#;
        let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
        match parse_datagram(&d).unwrap() {
            Message::PushData { rxpk, .. } => {
                assert_eq!(rxpk[0].rate, DataRate::Fsk { bps: 50000 });
                assert_eq!(rxpk[0].snr, None);
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn missing_stat_field_is_no_crc() {
        let j = r#"{"rxpk":[{"freq":868.1,"modu":"LORA","datr":"SF9BW125","codr":"4/5","rssi":-100,"lsnr":-3.5,"size":12}]}"#;
        let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
        match parse_datagram(&d).unwrap() {
            Message::PushData { rxpk, .. } => assert_eq!(rxpk[0].crc, Crc::None),
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn one_bad_rxpk_does_not_hide_the_others() {
        let j = include_str!("../tests/fixtures/rxpk_one_bad.json");
        let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
        match parse_datagram(&d).unwrap() {
            Message::PushData {
                rxpk, rxpk_errors, ..
            } => {
                assert_eq!(rxpk.len(), 2);
                assert_eq!(rxpk_errors, 2);
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn malformed_json_variants() {
        for j in [
            "",
            "{",
            "not json",
            "[1,2]",
            r#"{"rxpk":"x"}"#,
            r#"{"stat":[1]}"#,
            include_str!("../tests/fixtures/truncated.json"),
        ] {
            let d = build_datagram(2, 1, PUSH_DATA, Some(&EUI), j);
            assert_eq!(parse_datagram(&d), Err(ParseError::BadJson), "input {j:?}");
        }
    }

    #[test]
    fn push_data_without_full_eui() {
        assert_eq!(
            parse_datagram(&[2, 0, 0, PUSH_DATA, 1, 2, 3]),
            Err(ParseError::TooShort)
        );
    }

    #[test]
    fn pull_resp_txpk() {
        let j = include_str!("../tests/fixtures/pull_resp.json");
        let d = build_datagram(2, 9, PULL_RESP, None, j);
        match parse_datagram(&d).unwrap() {
            Message::PullResp { txpk: Some(t) } => {
                assert_eq!(t.rate, DataRate::Lora { sf: 9, bw_khz: 125 });
                assert!((t.freq_mhz - 869.525).abs() < 1e-9);
                assert_eq!(t.size, 33);
                assert_eq!(t.preamble, 8);
                assert!(t.no_crc);
                assert!(!t.imme);
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn pull_resp_garbage_is_error_but_bad_txpk_is_none() {
        let d = build_datagram(2, 9, PULL_RESP, None, "nope");
        assert_eq!(parse_datagram(&d), Err(ParseError::BadJson));
        let d = build_datagram(2, 9, PULL_RESP, None, r#"{"txpk":{"freq":869.5}}"#);
        assert_eq!(parse_datagram(&d), Ok(Message::PullResp { txpk: None }));
    }

    #[test]
    fn txpk_defaults_when_optional_fields_are_missing() {
        let j = r#"{"txpk":{"imme":true,"freq":868.1,"modu":"LORA","datr":"SF7BW125","size":10}}"#;
        let d = build_datagram(2, 9, PULL_RESP, None, j);
        match parse_datagram(&d).unwrap() {
            Message::PullResp { txpk: Some(t) } => {
                assert!(t.imme);
                assert!(!t.no_crc, "CRC stays on unless ncrc is set");
                assert_eq!(t.preamble, 8);
                assert_eq!(t.cr, 1);
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn tx_ack_variants() {
        let d = build_datagram(2, 9, TX_ACK, Some(&EUI), "");
        assert_eq!(
            parse_datagram(&d),
            Ok(Message::TxAck {
                eui: EUI,
                error: "NONE".into()
            })
        );
        let d = build_datagram(
            2,
            9,
            TX_ACK,
            Some(&EUI),
            r#"{"txpk_ack":{"error":"TOO_LATE"}}"#,
        );
        assert_eq!(
            parse_datagram(&d),
            Ok(Message::TxAck {
                eui: EUI,
                error: "TOO_LATE".into()
            })
        );
    }

    #[test]
    fn eui_formatting() {
        assert_eq!(eui_hex(&EUI), "aa555a0000000102");
    }
}

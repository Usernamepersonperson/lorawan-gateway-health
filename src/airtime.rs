//! LoRa time-on-air (Semtech SX1276 datasheet section 4.1.1.7 / AN1200.13) and an
//! EU868 duty-cycle sub-band table.

use crate::semtech::{DataRate, Rxpk, Txpk};

/// Parameters of one LoRa frame.
#[derive(Debug, Clone, Copy)]
pub struct LoraFrame {
    pub sf: u8,
    pub bw_khz: u32,
    /// 1..=4 for coding rate 4/5..4/8.
    pub cr: u8,
    /// Total PHY payload bytes (MAC header + payload + MIC).
    pub payload_bytes: u32,
    pub preamble_symbols: u32,
    pub explicit_header: bool,
    pub crc: bool,
}

/// Time on air in seconds.
pub fn lora_airtime_s(f: &LoraFrame) -> f64 {
    let sf = f64::from(f.sf);
    let bw = f64::from(f.bw_khz) * 1000.0;
    let t_sym = 2f64.powf(sf) / bw;
    // Low data rate optimisation is mandatory when the symbol time exceeds 16 ms.
    let de = if t_sym > 0.016 { 1.0 } else { 0.0 };
    let ih = if f.explicit_header { 0.0 } else { 1.0 };
    let crc = if f.crc { 1.0 } else { 0.0 };
    let t_preamble = (f64::from(f.preamble_symbols) + 4.25) * t_sym;
    let num = 8.0 * f64::from(f.payload_bytes) - 4.0 * sf + 28.0 + 16.0 * crc - 20.0 * ih;
    let den = 4.0 * (sf - 2.0 * de);
    let n_payload = 8.0 + ((num / den).ceil() * (f64::from(f.cr) + 4.0)).max(0.0);
    t_preamble + n_payload * t_sym
}

/// Airtime of a received uplink. Uplinks are explicit header with CRC, 8 preamble symbols
/// (the LoRaWAN default). FSK frames return `None`.
pub fn rxpk_airtime_s(p: &Rxpk) -> Option<f64> {
    match p.rate {
        DataRate::Lora { sf, bw_khz } => Some(lora_airtime_s(&LoraFrame {
            sf,
            bw_khz,
            cr: p.cr,
            payload_bytes: p.size,
            preamble_symbols: 8,
            explicit_header: true,
            crc: true,
        })),
        DataRate::Fsk { .. } => None,
    }
}

/// Airtime of a downlink ordered by PULL_RESP. LoRaWAN downlinks have no payload CRC (`ncrc`).
pub fn txpk_airtime_s(t: &Txpk) -> Option<f64> {
    match t.rate {
        DataRate::Lora { sf, bw_khz } => Some(lora_airtime_s(&LoraFrame {
            sf,
            bw_khz,
            cr: t.cr,
            payload_bytes: t.size,
            preamble_symbols: t.preamble,
            explicit_header: true,
            crc: !t.no_crc,
        })),
        DataRate::Fsk { bps } => {
            // 5 preamble + 3 sync + 1 length + payload + 2 CRC bytes, 8 bits each.
            let bits = (5 + 3 + 1 + t.size + 2) * 8;
            Some(f64::from(bits) / f64::from(bps.max(1)))
        }
    }
}

/// Duty-cycle regions the exporter knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Eu868,
    /// No regulatory table: one band, no limit shown.
    None,
}

impl Region {
    pub fn parse(s: &str) -> Option<Region> {
        match s.to_ascii_lowercase().as_str() {
            "eu868" => Some(Region::Eu868),
            "none" => Some(Region::None),
            _ => None,
        }
    }

    pub fn bands(self) -> &'static [Band] {
        match self {
            Region::Eu868 => EU868_BANDS,
            Region::None => NO_BANDS,
        }
    }

    /// Index into `bands()` for a carrier frequency in MHz.
    pub fn band_index(self, freq_mhz: f64) -> usize {
        let bands = self.bands();
        for (i, b) in bands.iter().enumerate() {
            if freq_mhz >= b.lo_mhz && freq_mhz < b.hi_mhz {
                return i;
            }
        }
        // Outside every table band: attribute to the last (catch-all) entry.
        bands.len() - 1
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub name: &'static str,
    pub lo_mhz: f64,
    pub hi_mhz: f64,
    /// Allowed fraction of time on air per hour (0.01 = 1 %). 0 = no limit.
    pub limit: f64,
}

/// ETSI EN 300 220 sub-bands as used by LoRaWAN EU868 (names g, g1, g2, g3, g4).
pub static EU868_BANDS: &[Band] = &[
    Band {
        name: "g",
        lo_mhz: 863.0,
        hi_mhz: 868.0,
        limit: 0.01,
    },
    Band {
        name: "g1",
        lo_mhz: 868.0,
        hi_mhz: 868.6,
        limit: 0.01,
    },
    Band {
        name: "g2",
        lo_mhz: 868.7,
        hi_mhz: 869.2,
        limit: 0.001,
    },
    Band {
        name: "g3",
        lo_mhz: 869.4,
        hi_mhz: 869.65,
        limit: 0.10,
    },
    Band {
        name: "g4",
        lo_mhz: 869.7,
        hi_mhz: 870.0,
        limit: 0.01,
    },
    Band {
        name: "other",
        lo_mhz: 0.0,
        hi_mhz: 0.0,
        limit: 0.0,
    },
];

static NO_BANDS: &[Band] = &[Band {
    name: "all",
    lo_mhz: 0.0,
    hi_mhz: 0.0,
    limit: 0.0,
}];

#[cfg(test)]
mod tests {
    use super::*;

    /// LoRaWAN uplink with `app` bytes of application payload: 13 B of MAC overhead
    /// (MHDR 1 + DevAddr 4 + FCtrl 1 + FCnt 2 + FPort 1 + MIC 4).
    fn lorawan_up(sf: u8, bw: u32, app: u32) -> f64 {
        lora_airtime_s(&LoraFrame {
            sf,
            bw_khz: bw,
            cr: 1,
            payload_bytes: app + 13,
            preamble_symbols: 8,
            explicit_header: true,
            crc: true,
        })
    }

    fn ms(s: f64) -> f64 {
        s * 1000.0
    }

    // TTN airtime calculator, EU868, 10 B application payload (23 B on air), CR 4/5, 125 kHz.
    #[test]
    fn ttn_sf7() {
        assert!((ms(lorawan_up(7, 125, 10)) - 61.7).abs() < 0.06);
    }
    #[test]
    fn ttn_sf8() {
        assert!((ms(lorawan_up(8, 125, 10)) - 113.2).abs() < 0.06);
    }
    #[test]
    fn ttn_sf9() {
        assert!((ms(lorawan_up(9, 125, 10)) - 205.8).abs() < 0.06);
    }
    #[test]
    fn ttn_sf10() {
        assert!((ms(lorawan_up(10, 125, 10)) - 370.7).abs() < 0.06);
    }
    #[test]
    fn ttn_sf11() {
        assert!((ms(lorawan_up(11, 125, 10)) - 823.3).abs() < 0.06);
    }
    #[test]
    fn ttn_sf12() {
        assert!((ms(lorawan_up(12, 125, 10)) - 1482.8).abs() < 0.06);
    }

    // Maximum-size SF12 frame (51 B application payload, 64 B on air): 2.793 s is the
    // figure usually quoted for the worst-case EU868 uplink.
    #[test]
    fn sf12_64_bytes() {
        assert!((lorawan_up(12, 125, 51) - 2.7935).abs() < 0.001);
    }

    #[test]
    fn sf7_64_bytes() {
        // 12.544 ms preamble + 103 symbols of 1.024 ms
        assert!((ms(lorawan_up(7, 125, 51)) - 118.016).abs() < 0.001);
    }

    #[test]
    fn airtime_grows_with_sf_and_payload() {
        let mut prev = 0.0;
        for sf in 7..=12 {
            let t = lorawan_up(sf, 125, 20);
            assert!(t > prev);
            prev = t;
        }
        assert!(lorawan_up(9, 125, 40) > lorawan_up(9, 125, 10));
    }

    #[test]
    fn wider_bandwidth_is_faster() {
        let t125 = lorawan_up(7, 125, 10);
        let t250 = lorawan_up(7, 250, 10);
        assert!((t125 / t250 - 2.0).abs() < 0.2);
    }

    #[test]
    fn low_data_rate_optimisation_threshold() {
        let with_de = lora_airtime_s(&LoraFrame {
            sf: 11,
            bw_khz: 125,
            cr: 1,
            payload_bytes: 23,
            preamble_symbols: 8,
            explicit_header: true,
            crc: true,
        });
        assert!((with_de - 0.8233).abs() < 0.001);
        // SF12 at 250 kHz has a 16.384 ms symbol, which also triggers it.
        let t = lora_airtime_s(&LoraFrame {
            sf: 12,
            bw_khz: 250,
            cr: 1,
            payload_bytes: 23,
            preamble_symbols: 8,
            explicit_header: true,
            crc: true,
        });
        // 12.25 * 16.384 ms preamble + (8 + ceil(180/40)*5 = 33) symbols
        assert!((t - (12.25 + 33.0) * 0.016384).abs() < 1e-6);
    }

    #[test]
    fn downlink_without_crc_is_shorter() {
        let mk = |no_crc| Txpk {
            freq_mhz: 869.525,
            rate: DataRate::Lora { sf: 9, bw_khz: 125 },
            cr: 1,
            size: 33,
            preamble: 8,
            no_crc,
            imme: false,
        };
        let a = txpk_airtime_s(&mk(true)).unwrap();
        let b = txpk_airtime_s(&mk(false)).unwrap();
        assert!(a <= b);
        // SF9, 33 B, no CRC: preamble 12.25 * 4.096 = 50.176 ms; payload 8 + ceil(256/36)*5 = 48 sym
        // -> 196.608 ms; total 246.784 ms
        assert!((a - 0.246784).abs() < 1e-6);
    }

    #[test]
    fn fsk_airtime() {
        let t = Txpk {
            freq_mhz: 868.8,
            rate: DataRate::Fsk { bps: 50000 },
            cr: 1,
            size: 20,
            preamble: 5,
            no_crc: false,
            imme: false,
        };
        // (5+3+1+20+2) bytes * 8 / 50 kbit/s
        assert!((txpk_airtime_s(&t).unwrap() - 0.00496).abs() < 1e-9);
    }

    #[test]
    fn eu868_band_lookup() {
        let r = Region::Eu868;
        let name = |f| r.bands()[r.band_index(f)].name;
        assert_eq!(name(867.1), "g");
        assert_eq!(name(868.1), "g1");
        assert_eq!(name(868.5), "g1");
        assert_eq!(name(868.8), "g2");
        assert_eq!(name(869.525), "g3");
        assert_eq!(name(869.85), "g4");
        assert_eq!(name(915.0), "other");
        assert_eq!(Region::None.band_index(915.0), 0);
    }

    #[test]
    fn region_parse() {
        assert_eq!(Region::parse("EU868"), Some(Region::Eu868));
        assert_eq!(Region::parse("none"), Some(Region::None));
        assert_eq!(Region::parse("us915"), None);
    }
}

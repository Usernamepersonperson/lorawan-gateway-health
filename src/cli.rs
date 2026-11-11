//! Command-line parsing (no dependency: the flag set is small).

use crate::airtime::Region;

pub const DEFAULT_METRICS_LISTEN: &str = "127.0.0.1:9089";
pub const DEFAULT_TAP_LISTEN: &str = "127.0.0.1:1701";
pub const DEFAULT_UPSTREAM: &str = "127.0.0.1:1700";

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub tap_listen: String,
    pub upstream: String,
    pub metrics_listen: String,
    pub region: Region,
    pub bridge_timeout_s: f64,
    pub mqtt_probe: Option<String>,
    pub mqtt_probe_interval_s: u64,
    pub disks: Vec<String>,
    pub temp_files: Vec<(String, String)>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            tap_listen: DEFAULT_TAP_LISTEN.into(),
            upstream: DEFAULT_UPSTREAM.into(),
            metrics_listen: DEFAULT_METRICS_LISTEN.into(),
            region: Region::Eu868,
            bridge_timeout_s: 60.0,
            mqtt_probe: None,
            mqtt_probe_interval_s: 30,
            disks: Vec::new(),
            temp_files: Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Parsed {
    Run(Box<Args>),
    Help,
    Version,
}

pub const USAGE: &str = "\
lgh-exporter - Prometheus exporter for a LoRaWAN gateway (Semtech UDP tap)

USAGE:
    lgh-exporter [OPTIONS]

The packet forwarder must send to --tap-listen instead of the bridge. Datagrams are relayed
unchanged to --upstream (and replies back), and parsed on the side.

OPTIONS:
    --tap-listen ADDR        UDP address the forwarder sends to      [127.0.0.1:1701]
    --upstream ADDR          UDP address of the bridge               [127.0.0.1:1700]
    --metrics-listen ADDR    HTTP address for /metrics               [127.0.0.1:9089]
    --region NAME            duty-cycle table: eu868 | none          [eu868]
    --bridge-timeout SECS    bridge/forwarder considered down after  [60]
    --mqtt-probe HOST:PORT   TCP-probe the MQTT broker (optional)
    --mqtt-probe-interval S  probe period in seconds                 [30]
    --disk PATH              filesystem to report, repeatable        [/]
    --temp-file LABEL=PATH   extra temperature file (millidegrees), repeatable
    -h, --help               print this help
    -V, --version            print the version
";

pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Parsed, String> {
    let mut a = Args::default();
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        // Accept both `--flag value` and `--flag=value`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--tap-listen" => a.tap_listen = value("--tap-listen")?,
            "--upstream" => a.upstream = value("--upstream")?,
            "--metrics-listen" => a.metrics_listen = value("--metrics-listen")?,
            "--region" => {
                let v = value("--region")?;
                a.region = Region::parse(&v)
                    .ok_or_else(|| format!("unknown region {v:?} (eu868 or none)"))?;
            }
            "--bridge-timeout" => {
                let v = value("--bridge-timeout")?;
                a.bridge_timeout_s = v
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite() && *x > 0.0)
                    .ok_or_else(|| format!("bad --bridge-timeout {v:?}"))?;
            }
            "--mqtt-probe" => a.mqtt_probe = Some(value("--mqtt-probe")?),
            "--mqtt-probe-interval" => {
                let v = value("--mqtt-probe-interval")?;
                a.mqtt_probe_interval_s = v
                    .parse::<u64>()
                    .ok()
                    .filter(|x| *x > 0)
                    .ok_or_else(|| format!("bad --mqtt-probe-interval {v:?}"))?;
            }
            "--disk" => a.disks.push(value("--disk")?),
            "--temp-file" => {
                let v = value("--temp-file")?;
                let (label, path) = v
                    .split_once('=')
                    .filter(|(l, p)| !l.is_empty() && !p.is_empty())
                    .ok_or_else(|| format!("--temp-file wants LABEL=PATH, got {v:?}"))?;
                a.temp_files.push((label.to_string(), path.to_string()));
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if a.disks.is_empty() {
        a.disks.push("/".into());
    }
    Ok(Parsed::Run(Box::new(a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    fn run(args: &[&str]) -> Args {
        match p(args).unwrap() {
            Parsed::Run(a) => *a,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let a = run(&[]);
        assert_eq!(a.metrics_listen, "127.0.0.1:9089");
        assert_eq!(a.tap_listen, "127.0.0.1:1701");
        assert_eq!(a.upstream, "127.0.0.1:1700");
        assert_eq!(a.disks, vec!["/".to_string()]);
        assert_eq!(a.region, Region::Eu868);
    }

    #[test]
    fn both_flag_styles() {
        let a = run(&[
            "--upstream",
            "10.0.0.1:1700",
            "--metrics-listen=0.0.0.0:9089",
        ]);
        assert_eq!(a.upstream, "10.0.0.1:1700");
        assert_eq!(a.metrics_listen, "0.0.0.0:9089");
    }

    #[test]
    fn repeatable_flags() {
        let a = run(&[
            "--disk",
            "/",
            "--disk",
            "/data",
            "--temp-file",
            "sx=/sys/x/temp",
        ]);
        assert_eq!(a.disks, vec!["/", "/data"]);
        assert_eq!(
            a.temp_files,
            vec![("sx".to_string(), "/sys/x/temp".to_string())]
        );
    }

    #[test]
    fn help_and_version() {
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert_eq!(p(&["-V"]), Ok(Parsed::Version));
    }

    #[test]
    fn errors() {
        assert!(p(&["--nope"]).is_err());
        assert!(p(&["--upstream"]).is_err());
        assert!(p(&["--region", "mars"]).is_err());
        assert!(p(&["--bridge-timeout", "0"]).is_err());
        assert!(p(&["--bridge-timeout", "abc"]).is_err());
        assert!(p(&["--temp-file", "nolabel"]).is_err());
        assert!(p(&["--mqtt-probe-interval", "0"]).is_err());
    }

    #[test]
    fn mqtt_probe_and_region() {
        let a = run(&[
            "--mqtt-probe",
            "broker:1883",
            "--region",
            "none",
            "--bridge-timeout",
            "30",
        ]);
        assert_eq!(a.mqtt_probe.as_deref(), Some("broker:1883"));
        assert_eq!(a.region, Region::None);
        assert_eq!(a.bridge_timeout_s, 30.0);
    }
}

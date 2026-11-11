//! Host readings from /proc, /sys and statvfs. Parsers take text so they are testable off-target.

use std::fs;
use std::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct Disk {
    pub path: String,
    pub total: u64,
    pub avail: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SysSnapshot {
    pub uptime_s: Option<f64>,
    pub load1: Option<f64>,
    pub mem_total: Option<u64>,
    pub mem_available: Option<u64>,
    pub cpu_busy_ratio: Option<f64>,
    pub disks: Vec<Disk>,
    pub temps: Vec<(String, f64)>,
    pub exporter_rss: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// First field of /proc/uptime.
pub fn parse_uptime(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}

/// First field of /proc/loadavg.
pub fn parse_load1(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}

/// (MemTotal, MemAvailable) in bytes from /proc/meminfo.
pub fn parse_meminfo(s: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| -> Option<u64> {
        let line = s.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

/// Aggregate "cpu " line of /proc/stat. busy = total - idle - iowait.
pub fn parse_cpu_times(s: &str) -> Option<CpuTimes> {
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|x| x.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Busy share between two samples; `None` when no time passed.
pub fn cpu_ratio(prev: Option<CpuTimes>, cur: CpuTimes) -> Option<f64> {
    let (b0, t0) = prev.map_or((0, 0), |p| (p.busy, p.total));
    let dt = cur.total.checked_sub(t0)?;
    if dt == 0 {
        return None;
    }
    Some(cur.busy.saturating_sub(b0) as f64 / dt as f64)
}

/// VmRSS from /proc/self/status, in bytes.
pub fn parse_rss(s: &str) -> Option<u64> {
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Kernel thermal files hold millidegrees Celsius. Small values are taken as degrees already.
pub fn parse_temperature(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    Some(if v.abs() >= 1000.0 { v / 1000.0 } else { v })
}

pub struct SysCollector {
    pub disks: Vec<String>,
    /// (label, file) pairs for sensors outside the thermal zones.
    pub temp_files: Vec<(String, String)>,
    prev_cpu: Mutex<Option<CpuTimes>>,
}

impl SysCollector {
    pub fn new(disks: Vec<String>, temp_files: Vec<(String, String)>) -> SysCollector {
        SysCollector {
            disks,
            temp_files,
            prev_cpu: Mutex::new(None),
        }
    }

    pub fn collect(&self) -> SysSnapshot {
        let read = |p: &str| fs::read_to_string(p).ok();
        let mut s = SysSnapshot {
            uptime_s: read("/proc/uptime").and_then(|t| parse_uptime(&t)),
            load1: read("/proc/loadavg").and_then(|t| parse_load1(&t)),
            exporter_rss: read("/proc/self/status").and_then(|t| parse_rss(&t)),
            ..Default::default()
        };
        if let Some(t) = read("/proc/meminfo") {
            let (total, avail) = parse_meminfo(&t);
            s.mem_total = total;
            s.mem_available = avail;
        }
        if let Some(cur) = read("/proc/stat").and_then(|t| parse_cpu_times(&t)) {
            let mut prev = self.prev_cpu.lock().unwrap_or_else(|e| e.into_inner());
            s.cpu_busy_ratio = cpu_ratio(*prev, cur);
            *prev = Some(cur);
        }
        for d in &self.disks {
            if let Some((total, avail)) = statvfs(d) {
                s.disks.push(Disk {
                    path: d.clone(),
                    total,
                    avail,
                });
            }
        }
        s.temps = thermal_zones();
        for (label, file) in &self.temp_files {
            if let Some(c) = read(file).and_then(|t| parse_temperature(&t)) {
                s.temps.push((label.clone(), c));
            }
        }
        s
    }
}

fn thermal_zones() -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/thermal") else {
        return out;
    };
    let mut dirs: Vec<_> = rd.filter_map(Result::ok).collect();
    dirs.sort_by_key(|e| e.file_name());
    for e in dirs {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let dir = e.path();
        let Some(c) = fs::read_to_string(dir.join("temp"))
            .ok()
            .and_then(|t| parse_temperature(&t))
        else {
            continue;
        };
        let kind = fs::read_to_string(dir.join("type"))
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        let label = if kind.is_empty() {
            name
        } else {
            format!("{name}:{kind}")
        };
        out.push((label, c));
    }
    out
}

#[cfg(unix)]
fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string and `st` is a properly sized, writable struct.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let frsize = st.f_frsize as u64;
        Some((st.f_blocks as u64 * frsize, st.f_bavail as u64 * frsize))
    }
}

#[cfg(not(unix))]
fn statvfs(_path: &str) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_and_load() {
        assert_eq!(parse_uptime("12345.67 9999.00\n"), Some(12345.67));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_uptime("abc 1"), None);
        assert_eq!(parse_load1("0.52 0.40 0.30 1/80 1234\n"), Some(0.52));
    }

    #[test]
    fn meminfo() {
        let t = "MemTotal:         506540 kB\nMemFree:          100000 kB\nMemAvailable:     312000 kB\n";
        assert_eq!(parse_meminfo(t), (Some(506540 * 1024), Some(312000 * 1024)));
        // Old kernels have no MemAvailable.
        assert_eq!(parse_meminfo("MemTotal: 10 kB\n"), (Some(10 * 1024), None));
        assert_eq!(parse_meminfo("garbage"), (None, None));
    }

    #[test]
    fn cpu_times() {
        let t = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 100 0 50 800 50 0 0 0 0 0\n";
        let c = parse_cpu_times(t).unwrap();
        assert_eq!(c.total, 1000);
        assert_eq!(c.busy, 150);
        assert_eq!(parse_cpu_times("cpu0 1 2 3 4\n"), None);
        assert_eq!(parse_cpu_times("cpu  1 2\n"), None);
    }

    #[test]
    fn cpu_ratio_between_samples() {
        let a = CpuTimes {
            busy: 150,
            total: 1000,
        };
        let b = CpuTimes {
            busy: 250,
            total: 1200,
        };
        assert_eq!(cpu_ratio(Some(a), b), Some(0.5));
        assert_eq!(cpu_ratio(None, a), Some(0.15));
        assert_eq!(cpu_ratio(Some(a), a), None);
        // counters going backwards (reboot between samples) -> no value rather than a bogus one
        assert_eq!(cpu_ratio(Some(b), a), None);
    }

    #[test]
    fn rss() {
        assert_eq!(
            parse_rss("Name:\tx\nVmRSS:\t    1234 kB\n"),
            Some(1234 * 1024)
        );
        assert_eq!(parse_rss("Name:\tx\n"), None);
    }

    #[test]
    fn temperature() {
        assert_eq!(parse_temperature("48500\n"), Some(48.5));
        assert_eq!(parse_temperature("37.5"), Some(37.5));
        assert_eq!(parse_temperature("hot"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn collector_reads_this_machine() {
        let c = SysCollector::new(vec!["/".into(), "/definitely/not/here".into()], vec![]);
        let s = c.collect();
        assert!(s.uptime_s.unwrap() > 0.0);
        assert!(s.mem_total.unwrap() > 0);
        assert_eq!(s.disks.len(), 1);
        assert!(s.disks[0].total >= s.disks[0].avail);
        assert!(s.exporter_rss.unwrap() > 0);
    }
}

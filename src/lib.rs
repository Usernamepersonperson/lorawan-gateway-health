//! lgh-exporter: gateway-side Prometheus exporter that taps the Semtech UDP
//! packet-forwarder protocol between a forwarder and a bridge.

pub mod airtime;
pub mod cli;
pub mod http;
pub mod metrics;
pub mod relay;
pub mod semtech;
pub mod sys;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall and monotonic clock pair used to stamp observations.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new()
    }
}

impl Clock {
    pub fn new() -> Clock {
        Clock {
            start: Instant::now(),
        }
    }

    pub fn now(&self) -> metrics::Now {
        metrics::Now {
            mono: self.start.elapsed().as_secs_f64(),
            unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
        }
    }
}

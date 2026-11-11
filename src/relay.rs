//! Transparent UDP relay between a packet forwarder and a bridge.
//!
//! Every datagram is forwarded byte for byte first and parsed afterwards, so a parser problem
//! can never disturb the data path. Each forwarder source address gets its own upstream
//! socket, so the bridge sees the same "one socket for uplink, one for downlink" pattern it
//! would see without the tap.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::metrics::Metrics;
use crate::Clock;

/// Upper bound on live forwarder source addresses (a forwarder uses two). When a new one
/// arrives and the table is full, the least recently used session is closed, so a forwarder
/// that restarts on new ports never gets locked out.
pub const MAX_SESSIONS: usize = 16;
const BUF: usize = 16 * 1024;

struct Session {
    sock: Arc<UdpSocket>,
    last_used: Instant,
    stop: Arc<AtomicBool>,
}

pub struct Relay {
    listen: Arc<UdpSocket>,
    upstream: SocketAddr,
    metrics: Arc<Metrics>,
    clock: Clock,
}

impl Relay {
    pub fn bind(
        listen: SocketAddr,
        upstream: SocketAddr,
        metrics: Arc<Metrics>,
        clock: Clock,
    ) -> io::Result<Relay> {
        Ok(Relay {
            listen: Arc::new(UdpSocket::bind(listen)?),
            upstream,
            metrics,
            clock,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listen.local_addr()
    }

    /// Blocking loop; returns only on a fatal socket error.
    pub fn run(self) -> io::Result<()> {
        let mut sessions: HashMap<SocketAddr, Session> = HashMap::new();
        let mut buf = vec![0u8; BUF];
        loop {
            let (n, client) = match self.listen.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Windows reports ICMP errors of earlier sends here; not fatal.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            };
            let data = &buf[..n];
            if !sessions.contains_key(&client) {
                if sessions.len() >= MAX_SESSIONS {
                    if let Some(oldest) = sessions
                        .iter()
                        .min_by_key(|(_, s)| s.last_used)
                        .map(|(k, _)| *k)
                    {
                        if let Some(old) = sessions.remove(&oldest) {
                            old.stop.store(true, Ordering::Relaxed);
                        }
                        self.metrics.relay_error("session_evicted");
                    }
                }
                match self.open_session(client) {
                    Ok(s) => {
                        sessions.insert(client, s);
                    }
                    Err(_) => {
                        self.metrics.relay_error("send_upstream");
                        continue;
                    }
                }
            }
            if let Some(s) = sessions.get_mut(&client) {
                s.last_used = Instant::now();
                if s.sock.send(data).is_err() {
                    self.metrics.relay_error("send_upstream");
                }
            }
            self.metrics.observe(data, self.clock.now());
        }
    }

    fn open_session(&self, client: SocketAddr) -> io::Result<Session> {
        let bind: SocketAddr = if self.upstream.is_ipv4() {
            "0.0.0.0:0".parse().expect("static address")
        } else {
            "[::]:0".parse().expect("static address")
        };
        let sock = UdpSocket::bind(bind)?;
        sock.connect(self.upstream)?;
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;
        let sock = Arc::new(sock);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let reader = sock.clone();
        let listen = self.listen.clone();
        let metrics = self.metrics.clone();
        let clock = self.clock;
        thread::Builder::new()
            .name("relay-down".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut buf = vec![0u8; BUF];
                while !stop_flag.load(Ordering::Relaxed) {
                    match reader.recv(&mut buf) {
                        Ok(n) => {
                            if listen.send_to(&buf[..n], client).is_err() {
                                metrics.relay_error("send_downstream");
                            }
                            metrics.observe(&buf[..n], clock.now());
                        }
                        // Bridge not listening yet: ICMP unreachable surfaces here once per send.
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            thread::sleep(Duration::from_millis(50));
                        }
                        // Read timeout (checks the stop flag) or a transient error.
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => thread::sleep(Duration::from_millis(500)),
                    }
                }
            })?;
        Ok(Session {
            sock,
            last_used: Instant::now(),
            stop,
        })
    }
}

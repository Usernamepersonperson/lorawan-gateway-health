use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lgh::cli::{self, Parsed};
use lgh::metrics::{Config, Metrics};
use lgh::relay::Relay;
use lgh::sys::SysCollector;
use lgh::{http, Clock};

fn resolve(what: &str, addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("{what} {addr:?}: {e}"))?
        .next()
        .ok_or_else(|| format!("{what} {addr:?}: no address"))
}

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(a)) => *a,
        Ok(Parsed::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("lgh-exporter {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: cli::Args) -> Result<(), String> {
    let tap = resolve("--tap-listen", &args.tap_listen)?;
    let upstream = resolve("--upstream", &args.upstream)?;
    let clock = Clock::new();
    let metrics = Arc::new(Metrics::new(Config {
        region: args.region,
        bridge_timeout_s: args.bridge_timeout_s,
    }));
    let sys = Arc::new(SysCollector::new(
        args.disks.clone(),
        args.temp_files.clone(),
    ));

    let relay = Relay::bind(tap, upstream, metrics.clone(), clock)
        .map_err(|e| format!("bind {tap}: {e}"))?;
    let listener = TcpListener::bind(&args.metrics_listen)
        .map_err(|e| format!("bind {}: {e}", args.metrics_listen))?;
    eprintln!(
        "lgh-exporter {}: tap {tap} -> {upstream}, metrics on http://{}/metrics",
        env!("CARGO_PKG_VERSION"),
        args.metrics_listen
    );

    if let Some(target) = args.mqtt_probe.clone() {
        let m = metrics.clone();
        let every = Duration::from_secs(args.mqtt_probe_interval_s);
        thread::Builder::new()
            .name("mqtt-probe".into())
            .stack_size(128 * 1024)
            .spawn(move || loop {
                let t0 = Instant::now();
                let ok = target
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut a| a.next())
                    .is_some_and(|addr| {
                        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok()
                    });
                m.set_mqtt_probe(ok, t0.elapsed().as_secs_f64(), clock.now().unix);
                thread::sleep(every);
            })
            .map_err(|e| e.to_string())?;
    }

    let render_metrics = metrics.clone();
    let render: Arc<http::RenderFn> =
        Arc::new(move || render_metrics.render(clock.now(), &sys.collect()));
    thread::Builder::new()
        .name("http-accept".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            if let Err(e) = http::serve(listener, render) {
                eprintln!("http server stopped: {e}");
                std::process::exit(1);
            }
        })
        .map_err(|e| e.to_string())?;

    relay.run().map_err(|e| format!("relay stopped: {e}"))
}

//! Minimal HTTP/1.1 server for /metrics and /healthz. One short-lived thread per connection,
//! GET and HEAD only, request head capped at 8 KiB.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub const CONTENT_TYPE_METRICS: &str = "text/plain; version=0.0.4; charset=utf-8";
const MAX_CONNECTIONS: usize = 16;
const MAX_HEAD: usize = 8 * 1024;

pub type RenderFn = dyn Fn() -> String + Send + Sync;

/// Response (status, content type, body, head_only) for a request line like `GET /metrics HTTP/1.1`.
pub fn route(request_line: &str, render: &RenderFn) -> (u16, &'static str, String, bool) {
    let mut it = request_line.split_whitespace();
    let (method, target) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let head_only = method == "HEAD";
    if method != "GET" && method != "HEAD" {
        return (405, "text/plain", "method not allowed\n".into(), false);
    }
    let path = target.split('?').next().unwrap_or("");
    match path {
        "/metrics" => (200, CONTENT_TYPE_METRICS, render(), head_only),
        "/healthz" => (200, "text/plain", "ok\n".into(), head_only),
        "/" => (
            200,
            "text/html; charset=utf-8",
            "<html><body><a href=\"/metrics\">/metrics</a></body></html>\n".into(),
            head_only,
        ),
        _ => (404, "text/plain", "not found\n".into(), head_only),
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

fn handle(mut s: TcpStream, render: &RenderFn) -> io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::new();
    let mut tmp = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && !head.windows(2).any(|w| w == b"\n\n") {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&tmp[..n]);
        if head.len() > MAX_HEAD {
            return write_response(&mut s, 431, "text/plain", "", true);
        }
    }
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next().unwrap_or("");
    let (code, ctype, body, head_only) = route(line, render);
    write_response(&mut s, code, ctype, &body, head_only)
}

fn write_response(
    s: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &str,
    head_only: bool,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    s.write_all(head.as_bytes())?;
    if !head_only {
        s.write_all(body.as_bytes())?;
    }
    s.flush()
}

/// Serve until the listener fails.
pub fn serve(listener: TcpListener, render: Arc<RenderFn>) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            continue; // dropped: the scraper retries
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (render, active) = (render.clone(), active.clone());
        let spawned = thread::Builder::new()
            .name("http".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _ = handle(stream, &*render);
                active.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            return Err(io::Error::other("cannot spawn http thread"));
        }
    }
    Ok(())
}

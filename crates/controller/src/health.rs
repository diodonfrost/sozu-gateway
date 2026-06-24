//! Minimal HTTP health endpoints for Kubernetes probes.
//!
//! `/healthz` (liveness) returns `200` as soon as the process serves. `/readyz`
//! (readiness) returns `200` only after the first successful reconcile — so the
//! Pod joins the Service (and receives traffic) only once Sōzu is programmed,
//! never during the cold-start "program gap". Readiness latches on: once set, a
//! later reconcile failure does not pull a live Pod out of rotation.
//!
//! Hand-rolled on a `TcpListener` to avoid pulling an HTTP server dependency for
//! two fixed responses; probes send a tiny request we only need the path from.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, warn};

/// Spawn the health server as a background task. On a bind failure it logs and
/// gives up (health is an operability aid, never a reason to kill routing).
pub fn spawn(addr: SocketAddr, ready: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, %addr, "failed to bind health endpoint");
                return;
            }
        };
        debug!(%addr, "health endpoints listening (/healthz, /readyz)");
        loop {
            match listener.accept().await {
                Ok((mut sock, _)) => {
                    let ready = ready.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_one(&mut sock, &ready).await {
                            debug!(error = %e, "health connection error");
                        }
                    });
                }
                Err(e) => warn!(error = %e, "health accept error"),
            }
        }
    });
}

async fn serve_one(sock: &mut TcpStream, ready: &AtomicBool) -> std::io::Result<()> {
    // Probes send a tiny request; one read is enough to see the request line.
    let mut buf = [0u8; 256];
    let n = sock.read(&mut buf).await?;
    let (status, body): (&str, &str) = match request_path(&buf[..n]) {
        Some("/readyz") => {
            if ready.load(Ordering::Relaxed) {
                ("200 OK", "ready")
            } else {
                ("503 Service Unavailable", "not ready")
            }
        }
        Some("/healthz") => ("200 OK", "ok"),
        _ => ("404 Not Found", "not found"),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(response.as_bytes()).await?;
    sock.flush().await
}

/// Extract the request-target path from an HTTP request line
/// (`GET /readyz HTTP/1.1`), stripping any query string.
fn request_path(head: &[u8]) -> Option<&str> {
    let end = head
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(head.len());
    let line = std::str::from_utf8(&head[..end]).ok()?;
    let mut parts = line.split(' ');
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target.split('?').next().unwrap_or(target))
}

#[cfg(test)]
mod tests {
    use super::request_path;

    #[test]
    fn parses_request_target() {
        assert_eq!(
            request_path(b"GET /readyz HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/readyz")
        );
        assert_eq!(
            request_path(b"GET /healthz?x=1 HTTP/1.1\r\n"),
            Some("/healthz")
        );
        assert_eq!(request_path(b"GET / HTTP/1.1\r\n"), Some("/"));
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert_eq!(request_path(b""), None);
        assert_eq!(request_path(b"garbage"), None);
    }
}

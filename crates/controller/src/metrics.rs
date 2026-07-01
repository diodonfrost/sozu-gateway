//! Minimal HTTP `/metrics` endpoint exposing Sōzu's data-plane metrics.
//!
//! Sōzu has no native Prometheus endpoint; on each scrape this handler pulls its
//! aggregated metrics over the command socket (a `QueryMetrics` request) and
//! renders them with the pure `sozu-gw-prometheus` crate. Best-effort: a socket
//! hiccup yields `503`, never a panic, and a bind failure simply disables the
//! endpoint — routing is never affected.
//!
//! Hand-rolled on a `TcpListener` for the same reason as the health server, and
//! it reuses that module's request-line parser.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sozu_gw_agent::{QueryMetricsOptions, SozuAgentHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

/// Prometheus text exposition content type (legacy `0.0.4`).
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Bound on one scrape's `QueryMetrics` round-trip. The query shares the single
/// FIFO socket-worker with routing applies, so an unbounded await under a hung
/// socket would park one tokio task per scrape forever. Note the bound is on
/// *our* wait: a timed-out job still completes on the worker thread eventually.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the metrics server as a background task. On a bind failure it logs and
/// gives up (metrics are an operability aid, never a reason to kill routing).
pub fn spawn(addr: SocketAddr, agent: SozuAgentHandle) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, %addr, "failed to bind metrics endpoint");
                return;
            }
        };
        debug!(%addr, "metrics endpoint listening (/metrics)");
        // At most one scrape in flight: concurrent scrapers would stack
        // `QueryMetrics` jobs in the socket-worker queue ahead of routing
        // applies. A busy scrape gets an immediate 503 (Prometheus retries on
        // its next cycle) instead of queueing.
        let scrape_permit = Arc::new(Semaphore::new(1));
        loop {
            match listener.accept().await {
                Ok((mut sock, _)) => {
                    let agent = agent.clone();
                    let scrape_permit = scrape_permit.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_one(&mut sock, &agent, &scrape_permit).await {
                            debug!(error = %e, "metrics connection error");
                        }
                    });
                }
                Err(e) => warn!(error = %e, "metrics accept error"),
            }
        }
    });
}

async fn serve_one(
    sock: &mut TcpStream,
    agent: &SozuAgentHandle,
    scrape_permit: &Semaphore,
) -> std::io::Result<()> {
    let mut buf = [0u8; 256];
    let n = sock.read(&mut buf).await?;
    let (status, content_type, body): (&str, &str, String) =
        match crate::health::request_path(&buf[..n]) {
            Some("/metrics") => metrics_response(agent, scrape_permit).await,
            _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
        };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(response.as_bytes()).await?;
    sock.flush().await
}

/// Build the `/metrics` response: one permit-gated, time-bounded `QueryMetrics`
/// round-trip. Every failure mode (busy, socket error, timeout) is a `503`,
/// never a panic — the endpoint stays best-effort and orthogonal to routing.
async fn metrics_response(
    agent: &SozuAgentHandle,
    scrape_permit: &Semaphore,
) -> (&'static str, &'static str, String) {
    // `try_acquire`, not `acquire`: a second scraper must fail fast, not park
    // behind the first one (that queue growth is exactly the failure mode).
    let Ok(_permit) = scrape_permit.try_acquire() else {
        debug!("metrics scrape rejected: another scrape is in flight");
        return (
            "503 Service Unavailable",
            "text/plain",
            "another scrape is in flight\n".to_string(),
        );
    };
    match tokio::time::timeout(
        QUERY_TIMEOUT,
        agent.query_metrics(QueryMetricsOptions::default()),
    )
    .await
    {
        Ok(Ok(metrics)) => ("200 OK", CONTENT_TYPE, sozu_gw_prometheus::render(&metrics)),
        Ok(Err(e)) => {
            warn!(error = %e, "failed to query sozu metrics");
            (
                "503 Service Unavailable",
                "text/plain",
                "metrics unavailable\n".to_string(),
            )
        }
        Err(_elapsed) => {
            warn!("sozu metrics query timed out");
            (
                "503 Service Unavailable",
                "text/plain",
                "metrics unavailable\n".to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the single permit held, a scrape must 503 immediately without ever
    /// reaching the socket worker; once released, the query goes through (and
    /// fails on the missing socket via the ordinary unavailable path).
    #[tokio::test]
    async fn busy_scrape_is_rejected_immediately_without_queueing() {
        let agent = SozuAgentHandle::spawn("/nonexistent/sozu.sock").expect("spawn agent");
        let scrape_permit = Semaphore::new(1);

        let held = scrape_permit.try_acquire().expect("hold the only permit");
        let (status, _ct, body) = metrics_response(&agent, &scrape_permit).await;
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(
            body, "another scrape is in flight\n",
            "a busy scrape must take the immediate-rejection path, not queue a job"
        );

        drop(held);
        let (status, _ct, body) = metrics_response(&agent, &scrape_permit).await;
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(
            body, "metrics unavailable\n",
            "with the permit free the query must reach the agent"
        );
    }
}

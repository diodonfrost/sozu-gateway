//! Minimal HTTP `/metrics` endpoint: controller self-metrics + Sōzu's
//! data-plane metrics.
//!
//! Sōzu has no native Prometheus endpoint; on each scrape this handler pulls its
//! aggregated metrics over the command socket (a `QueryMetrics` request) and
//! renders them with the pure `sozu-gw-prometheus` crate, prefixed by the
//! controller's own [`SelfMetrics`]. Best-effort: a socket
//! hiccup yields `503`, never a panic, and a bind failure simply disables the
//! endpoint — routing is never affected.
//!
//! Hand-rolled on a `TcpListener` for the same reason as the health server, and
//! it reuses that module's request-line parser.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sozu_gw_agent::{QueryMetricsOptions, SozuAgentHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

/// Prometheus text exposition content type (legacy `0.0.4`).
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Controller self-metrics: the signals that make the loop's own failure
/// modes *visible*. The recoverable outages (a wedged apply, a Sōzu restart,
/// reconcile churn) are silent from the data plane's point of view — the pod
/// stays Ready and `/metrics` keeps serving Sōzu's numbers — so operators
/// need the controller to report on itself: when it last applied
/// successfully, how often it fails, and how often it had to reset the
/// shadow. Updated from the reconcile loop with relaxed atomics (monitoring
/// must never contend with routing).
#[derive(Default)]
pub struct SelfMetrics {
    reconciles_total: AtomicU64,
    reconcile_failures_total: AtomicU64,
    shadow_resets_total: AtomicU64,
    last_success_unix_seconds: AtomicI64,
    last_reconcile_duration_ms: AtomicU64,
}

impl SelfMetrics {
    /// Record one reconcile pass (duration + outcome). A successful pass also
    /// advances the last-success timestamp — the single most useful signal:
    /// "how stale is the programmed state allowed to be" is an alert rule away.
    pub fn record_reconcile(&self, duration: Duration, success: bool) {
        self.reconciles_total.fetch_add(1, Ordering::Relaxed);
        self.last_reconcile_duration_ms
            .store(duration.as_millis() as u64, Ordering::Relaxed);
        if success {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.last_success_unix_seconds.store(now, Ordering::Relaxed);
        } else {
            self.reconcile_failures_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a shadow reset (a detected Sōzu restart → full re-apply due).
    pub fn record_shadow_reset(&self) {
        self.shadow_resets_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Text exposition of the self-metrics, prepended to the data-plane
    /// exposition on each scrape.
    pub fn render(&self) -> String {
        let duration_seconds =
            self.last_reconcile_duration_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        format!(
            "# HELP sozu_gw_controller_reconciles_total Reconcile passes attempted.\n\
             # TYPE sozu_gw_controller_reconciles_total counter\n\
             sozu_gw_controller_reconciles_total {}\n\
             # HELP sozu_gw_controller_reconcile_failures_total Reconcile passes that failed (build, translate or apply).\n\
             # TYPE sozu_gw_controller_reconcile_failures_total counter\n\
             sozu_gw_controller_reconcile_failures_total {}\n\
             # HELP sozu_gw_controller_shadow_resets_total Shadow resets after a detected Sōzu restart (each one triggers a full re-apply).\n\
             # TYPE sozu_gw_controller_shadow_resets_total counter\n\
             sozu_gw_controller_shadow_resets_total {}\n\
             # HELP sozu_gw_controller_last_successful_reconcile_timestamp_seconds Unix time of the last fully successful reconcile (0 until the first one).\n\
             # TYPE sozu_gw_controller_last_successful_reconcile_timestamp_seconds gauge\n\
             sozu_gw_controller_last_successful_reconcile_timestamp_seconds {}\n\
             # HELP sozu_gw_controller_last_reconcile_duration_seconds Duration of the most recent reconcile pass.\n\
             # TYPE sozu_gw_controller_last_reconcile_duration_seconds gauge\n\
             sozu_gw_controller_last_reconcile_duration_seconds {}\n",
            self.reconciles_total.load(Ordering::Relaxed),
            self.reconcile_failures_total.load(Ordering::Relaxed),
            self.shadow_resets_total.load(Ordering::Relaxed),
            self.last_success_unix_seconds.load(Ordering::Relaxed),
            duration_seconds,
        )
    }
}

/// Bound on one scrape's `QueryMetrics` round-trip. The query shares the single
/// FIFO socket-worker with routing applies, so an unbounded await under a hung
/// socket would park one tokio task per scrape forever. Note the bound is on
/// *our* wait: a timed-out job still completes on the worker thread eventually.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the metrics server as a background task. On a bind failure it logs and
/// gives up (metrics are an operability aid, never a reason to kill routing).
pub fn spawn(addr: SocketAddr, agent: SozuAgentHandle, self_metrics: Arc<SelfMetrics>) {
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
                    let self_metrics = self_metrics.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            serve_one(&mut sock, &agent, &scrape_permit, &self_metrics).await
                        {
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
    self_metrics: &SelfMetrics,
) -> std::io::Result<()> {
    let mut buf = [0u8; 256];
    let n = sock.read(&mut buf).await?;
    let (status, content_type, body): (&str, &str, String) =
        match crate::health::request_path(&buf[..n]) {
            Some("/metrics") => metrics_response(agent, scrape_permit, self_metrics).await,
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
    self_metrics: &SelfMetrics,
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
        Ok(Ok(metrics)) => (
            "200 OK",
            CONTENT_TYPE,
            // Self-metrics first, then the data plane's. The Sōzu-down case
            // stays a 503 (documented; Prometheus flags the target down —
            // loud); the self-metrics matter for the *silent* modes, where
            // scrapes still succeed while reconciliation is stuck.
            format!(
                "{}{}",
                self_metrics.render(),
                sozu_gw_prometheus::render(&metrics)
            ),
        ),
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
        let self_metrics = SelfMetrics::default();

        let held = scrape_permit.try_acquire().expect("hold the only permit");
        let (status, _ct, body) = metrics_response(&agent, &scrape_permit, &self_metrics).await;
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(
            body, "another scrape is in flight\n",
            "a busy scrape must take the immediate-rejection path, not queue a job"
        );

        drop(held);
        let (status, _ct, body) = metrics_response(&agent, &scrape_permit, &self_metrics).await;
        assert_eq!(status, "503 Service Unavailable");
        assert_eq!(
            body, "metrics unavailable\n",
            "with the permit free the query must reach the agent"
        );
    }

    #[test]
    fn self_metrics_render_tracks_outcomes() {
        let m = SelfMetrics::default();
        let r = m.render();
        assert!(r.contains("sozu_gw_controller_reconciles_total 0\n"));
        assert!(r.contains("sozu_gw_controller_last_successful_reconcile_timestamp_seconds 0\n"));

        m.record_reconcile(Duration::from_millis(250), false);
        m.record_reconcile(Duration::from_millis(500), true);
        m.record_shadow_reset();
        let r = m.render();
        assert!(r.contains("sozu_gw_controller_reconciles_total 2\n"));
        assert!(r.contains("sozu_gw_controller_reconcile_failures_total 1\n"));
        assert!(r.contains("sozu_gw_controller_shadow_resets_total 1\n"));
        assert!(r.contains("sozu_gw_controller_last_reconcile_duration_seconds 0.5\n"));
        // The last-success timestamp moved off zero on the successful pass.
        assert!(!r.contains("last_successful_reconcile_timestamp_seconds 0\n"));
    }
}

//! sozu-agent: thin, typed wrapper around `sozu-command-lib`'s command socket.
//!
//! Owns all socket I/O: connect, send a batch request-by-request (each acked
//! through Sōzu's `Processing → Ok/Failure` reply sequence), bounded reads
//! *and writes* (no permanent hang), and reconnect-and-retry on a broken
//! channel. The requests themselves are NOT all idempotent: convergence
//! against a drifted Sōzu comes from the upsert semantics of
//! `AddCluster`/`AddBackend` plus this crate's tolerance for already-gone
//! teardowns and its remove + re-add repair of duplicate frontend adds
//! (see [`SozuAgent::apply`]).
//!
//! Two layers:
//!  - [`SozuAgent`] — the synchronous core (the command socket is a blocking,
//!    single-stream protocol; this type owns it).
//!  - [`SozuAgentHandle`] — an async, cloneable handle. It runs the blocking
//!    core on a dedicated thread and serialises all access through an mpsc
//!    queue, so concurrent async callers never share the socket unsafely.
#![forbid(unsafe_code)]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, Request, Response, ResponseContent,
    ResponseStatus, Status,
};
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::{debug, warn};

// Re-exported so callers (the controller) can drive `query_metrics` without
// taking a direct dependency on `sozu-command-lib`.
pub use sozu_command_lib::proto::command::{AggregatedMetrics, QueryMetricsOptions};

/// Client-side socket buffer sizes (server bounds come from `config.toml`).
const DEFAULT_BUFFER_SIZE: u64 = 1024 * 1024;
const DEFAULT_MAX_BUFFER_SIZE: u64 = 16 * 1024 * 1024;
/// Upper bound on a whole request's ack sequence, so a wedged Sōzu can't hang
/// us forever (applies across the Processing→Ok replies, not per read).
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Small backoff before a reconnect-and-retry, so an unhealthy Sōzu is not
/// hammered with reconnect storms across reconcile cycles.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Whether a request is a teardown verb (`Remove*` / `DeactivateListener`).
/// Sōzu rejects removing (or deactivating) an object it no longer holds, so a
/// failed teardown is treated as an already-done no-op rather than wedging
/// reconciliation (see [`SozuAgent::apply`]).
fn is_teardown(request: &Request) -> bool {
    matches!(
        &request.request_type,
        Some(
            RequestType::RemoveCluster(_)
                | RequestType::RemoveBackend(_)
                | RequestType::RemoveHttpFrontend(_)
                | RequestType::RemoveHttpsFrontend(_)
                | RequestType::RemoveTcpFrontend(_)
                | RequestType::RemoveUdpFrontend(_)
                | RequestType::RemoveListener(_)
                | RequestType::RemoveCertificate(_)
                | RequestType::DeactivateListener(_)
        )
    )
}

/// Whether a failed request is an add Sōzu rejected because it already holds
/// an object under the same key (`StateError::Exists`).
///
/// Sōzu's frontend and L4-listener `Add*` verbs are NOT idempotent (unlike
/// `AddCluster`/`AddBackend`, which upsert, and `AddCertificate`, which skips a
/// cert it already holds): re-applying one returns `StateError::Exists`
/// ("… already exists") — a hard `Failure` on the wire. That happens whenever
/// Sōzu applied a batch our shadow never recorded (an ack that arrived after
/// the read deadline, a partially applied batch, a controller restart
/// mid-apply). Failing the batch would re-emit the same add every reconcile,
/// forever. What happens instead depends on the verb (see
/// [`SozuAgent::apply`]): frontend adds are *repaired* with a remove + re-add
/// (see [`removal_for`] for why tolerance is not enough), listener adds are
/// tolerated as already-applied.
///
/// Scoped to the verbs where `Exists` is the expected duplicate outcome, and
/// to failure messages that actually carry `StateError::Exists`'s Display
/// ("{kind:?} '{id}' already exists"), matched case-insensitively because the
/// main process wraps it in its own error prefix. `ActivateListener` is *not*
/// here: re-activation is idempotent in Sōzu (it only fails `NotFound`).
fn is_duplicate_add(request: &Request, failure_message: &str) -> bool {
    let exists_is_duplicate = matches!(
        &request.request_type,
        Some(
            RequestType::AddHttpFrontend(_)
                | RequestType::AddHttpsFrontend(_)
                | RequestType::AddTcpFrontend(_)
                | RequestType::AddUdpFrontend(_)
                | RequestType::AddTcpListener(_)
                | RequestType::AddUdpListener(_)
        )
    );
    exists_is_duplicate && failure_message.to_lowercase().contains("already exist")
}

/// The `Remove*` that evicts whatever Sōzu holds under the same key as this
/// add — the repair for a duplicate *frontend* add.
///
/// An HTTP/HTTPS frontend `Exists` is keyed by the route key alone
/// (`address;hostname;path[;method]` — `RequestHttpFrontend`'s `Display`):
/// `cluster_id`, tags and the filter fields are NOT compared. So "already
/// exists" can mean the stored route points at a *different* cluster than the
/// one being added; merely tolerating it would advance the shadow to the
/// desired cluster while Sōzu keeps routing to the stale one — permanent
/// silent misrouting. The removes are keyed the same way (verified in
/// `sozu-command-lib` 2.1.0's `state.rs`: `remove_http_frontend` /
/// `remove_https_frontend` remove by `front.to_string()`, the route key, not
/// by struct equality; `remove_{tcp,udp}_frontend` match the `cluster_id`
/// bucket by address, and their adds only ever return `Exists` for an
/// identical entry), so a remove built from the add's own payload evicts the
/// stored entry even when it differs — then re-sending the add installs the
/// desired one.
///
/// `AddTcpListener`/`AddUdpListener` return `None` deliberately: a listener is
/// keyed by its address alone, so a duplicate cannot mask a different routing
/// target, and repairing one would need deactivate-before-remove ordering —
/// out of scope. A duplicate listener add stays plainly tolerated.
fn removal_for(request: &Request) -> Option<Request> {
    match &request.request_type {
        Some(RequestType::AddHttpFrontend(front)) => {
            Some(RequestType::RemoveHttpFrontend(front.clone()).into())
        }
        Some(RequestType::AddHttpsFrontend(front)) => {
            Some(RequestType::RemoveHttpsFrontend(front.clone()).into())
        }
        Some(RequestType::AddTcpFrontend(front)) => {
            Some(RequestType::RemoveTcpFrontend(front.clone()).into())
        }
        Some(RequestType::AddUdpFrontend(front)) => {
            Some(RequestType::RemoveUdpFrontend(front.clone()).into())
        }
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum SozuError {
    #[error("sozu command channel error: {0}")]
    Channel(String),
    #[error("sozu rejected the request: {0}")]
    Failure(String),
    #[error("sozu returned an unexpected response (no metrics content)")]
    UnexpectedResponse,
    #[error("sozu-agent worker thread is gone")]
    WorkerGone,
}

/// Synchronous client for the Sōzu command socket. Reconnects lazily.
pub struct SozuAgent {
    path: String,
    buffer_size: u64,
    max_buffer_size: u64,
    read_timeout: Duration,
    channel: Option<Channel<Request, Response>>,
}

impl SozuAgent {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            read_timeout: DEFAULT_READ_TIMEOUT,
            channel: None,
        }
    }

    fn connect(&mut self) -> Result<(), SozuError> {
        debug!(path = %self.path, "connecting to sozu command socket");
        let mut channel: Channel<Request, Response> =
            Channel::from_path(&self.path, self.buffer_size, self.max_buffer_size)
                .map_err(|e| SozuError::Channel(format!("connect: {e:?}")))?;
        // Blocking mode is required: a non-blocking `write_message` only buffers,
        // it does not flush to the socket.
        channel
            .blocking()
            .map_err(|e| SozuError::Channel(format!("set blocking: {e:?}")))?;
        self.channel = Some(channel);
        Ok(())
    }

    fn channel_mut(&mut self) -> Result<&mut Channel<Request, Response>, SozuError> {
        if self.channel.is_none() {
            self.connect()?;
        }
        self.channel
            .as_mut()
            .ok_or_else(|| SozuError::Channel("not connected".to_string()))
    }

    /// Send one request and await its terminal response, skipping interim
    /// `Processing` replies.
    fn send_one(
        channel: &mut Channel<Request, Response>,
        read_timeout: Duration,
        request: &Request,
    ) -> Result<Response, SozuError> {
        channel
            .write_message(request)
            .map_err(|e| SozuError::Channel(format!("write: {e:?}")))?;
        // One deadline for the whole Processing→Ok sequence.
        let deadline = Instant::now() + read_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SozuError::Channel(
                    "timed out waiting for a terminal response".to_string(),
                ));
            }
            let response = channel
                .read_message_blocking_timeout(Some(remaining))
                .map_err(|e| SozuError::Channel(format!("read: {e:?}")))?;
            let status = response.status;
            if status == ResponseStatus::Processing as i32 {
                continue;
            }
            if status == ResponseStatus::Ok as i32 {
                return Ok(response);
            }
            return Err(SozuError::Failure(response.message));
        }
    }

    /// Send one request and return its terminal `Response`; on a *channel* error
    /// (broken pipe, etc.) reconnect once and retry. Application-level `Failure`
    /// is returned without retrying.
    fn send_with_retry(&mut self, request: &Request) -> Result<Response, SozuError> {
        let read_timeout = self.read_timeout;
        let first = {
            let channel = self.channel_mut()?;
            Self::send_one(channel, read_timeout, request)
        };
        match first {
            Ok(response) => Ok(response),
            Err(failure @ SozuError::Failure(_)) => Err(failure),
            Err(channel_error) => {
                warn!(error = %channel_error, "sozu channel error, reconnecting and retrying");
                self.channel = None;
                thread::sleep(RECONNECT_BACKOFF);
                let channel = self.channel_mut()?;
                Self::send_one(channel, read_timeout, request)
            }
        }
    }

    /// Apply one request, discarding the response body (mutations only care about
    /// success/failure).
    fn apply_one(&mut self, request: &Request) -> Result<(), SozuError> {
        self.send_with_retry(request).map(|_| ())
    }

    /// Apply a batch of requests in order (the caller supplies a dependency-safe
    /// order, e.g. from the Translator). Stops at the first error — except a
    /// failed *teardown* (see [`is_teardown`]), which is tolerated, and a
    /// *duplicate add* (see [`is_duplicate_add`]): a duplicate frontend add is
    /// repaired with a remove + re-add (see [`removal_for`]), a duplicate L4
    /// listener add is tolerated.
    pub fn apply(&mut self, requests: &[Request]) -> Result<(), SozuError> {
        for request in requests {
            match self.apply_one(request) {
                Ok(()) => {}
                // Sōzu's teardown verbs are NOT idempotent: removing an object it
                // no longer holds (state diverged from our shadow — a partially
                // applied batch, or a worker-side drop) is a hard `Failure`. Such a
                // teardown is effectively already done, so treat it as a no-op.
                // This keeps the invariant that re-diffing from the shadow
                // converges: without it, one un-removable object wedges *all*
                // reconciliation forever (the same failing remove re-emitted
                // every cycle).
                Err(SozuError::Failure(msg)) if is_teardown(request) => {
                    warn!(error = %msg, "sozu rejected a teardown; treating it as already-gone so reconciliation converges");
                }
                // The mirror image: an add Sōzu already holds (the shadow missed
                // an applied batch) fails with `Exists` on every re-application,
                // wedging reconciliation the same way. For a *frontend* this is
                // NOT proof the desired object is there — the `Exists` key
                // excludes `cluster_id`/tags/filters, so the stored route may
                // point at a different cluster (see [`removal_for`]). Repair it:
                // evict whatever occupies the key, then re-send our add.
                Err(SozuError::Failure(msg)) if is_duplicate_add(request, &msg) => {
                    match removal_for(request) {
                        Some(remove) => {
                            warn!(error = %msg, "sozu already holds this frontend's route key; repairing with a remove + re-add");
                            match self.apply_one(&remove) {
                                Ok(()) => {}
                                // The stored entry vanished between the `Exists`
                                // and our remove: harmless — the re-add decides.
                                Err(SozuError::Failure(remove_msg)) => {
                                    warn!(error = %remove_msg, "sozu rejected the repair remove; attempting the re-add anyway");
                                }
                                Err(channel_error) => return Err(channel_error),
                            }
                            // One repair attempt only: if the re-add fails — even
                            // with another `Exists` — fail the batch, so the
                            // shadow stays put and the next reconcile retries
                            // from the unchanged baseline.
                            self.apply_one(request)?;
                        }
                        // L4 listener adds: address-keyed, a duplicate cannot
                        // mask a different target — tolerate as already-applied
                        // (repair would need deactivate-before-remove ordering,
                        // out of scope here).
                        None => {
                            warn!(error = %msg, "sozu rejected a duplicate listener add; treating it as already-applied so reconciliation converges");
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Liveness check: `Status` round-trip.
    pub fn status(&mut self) -> Result<(), SozuError> {
        self.apply_one(&RequestType::Status(Status {}).into())
    }

    /// Ask Sōzu to load its routing state from a file path (visible to Sōzu).
    pub fn load_state(&mut self, path: impl Into<String>) -> Result<(), SozuError> {
        self.apply_one(&RequestType::LoadState(path.into()).into())
    }

    /// Ask Sōzu to persist its current routing state to a file path.
    pub fn save_state(&mut self, path: impl Into<String>) -> Result<(), SozuError> {
        self.apply_one(&RequestType::SaveState(path.into()).into())
    }

    /// Pull Sōzu's aggregated metrics over the command socket (a `QueryMetrics`
    /// round-trip). Unlike a mutation, this keeps the response body and extracts
    /// the `AggregatedMetrics` from it.
    pub fn query_metrics(
        &mut self,
        options: QueryMetricsOptions,
    ) -> Result<AggregatedMetrics, SozuError> {
        let request: Request = RequestType::QueryMetrics(options).into();
        let response = self.send_with_retry(&request)?;
        match response.content {
            Some(ResponseContent {
                content_type: Some(ContentType::Metrics(metrics)),
            }) => Ok(metrics),
            _ => Err(SozuError::UnexpectedResponse),
        }
    }
}

// ----------------------------------------------------------------------------
// Async handle
// ----------------------------------------------------------------------------

enum Job {
    Apply(Vec<Request>, oneshot::Sender<Result<(), SozuError>>),
    SaveState(String, oneshot::Sender<Result<(), SozuError>>),
    QueryMetrics(
        QueryMetricsOptions,
        oneshot::Sender<Result<AggregatedMetrics, SozuError>>,
    ),
}

/// Cloneable async handle to a single Sōzu command socket. All work runs on one
/// dedicated thread, so socket access is serialised across clones.
#[derive(Clone)]
pub struct SozuAgentHandle {
    tx: mpsc::Sender<Job>,
}

impl SozuAgentHandle {
    /// Spawn the worker thread for the socket at `path`. The connection is
    /// established lazily on first use (so this never fails on a not-yet-ready
    /// Sōzu).
    pub fn spawn(path: impl Into<String>) -> std::io::Result<Self> {
        let path = path.into();
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("sozu-agent".to_string())
            .spawn(move || {
                let mut agent = SozuAgent::new(path);
                // Ends when every `SozuAgentHandle` (and thus every Sender) drops.
                for job in rx {
                    match job {
                        Job::Apply(requests, reply) => {
                            let _ = reply.send(agent.apply(&requests));
                        }
                        Job::SaveState(path, reply) => {
                            let _ = reply.send(agent.save_state(path));
                        }
                        Job::QueryMetrics(options, reply) => {
                            let _ = reply.send(agent.query_metrics(options));
                        }
                    }
                }
                debug!("sozu-agent worker thread exiting");
            })?;
        Ok(Self { tx })
    }

    /// Apply a batch of requests, awaiting Sōzu's acks.
    pub async fn apply(&self, requests: Vec<Request>) -> Result<(), SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::Apply(requests, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }

    /// Ask Sōzu to dump its full routing state to `path` (a file both the
    /// controller and Sōzu can see via the shared volume).
    pub async fn save_state(&self, path: String) -> Result<(), SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::SaveState(path, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }

    /// Pull Sōzu's aggregated metrics over the command socket.
    pub async fn query_metrics(
        &self,
        options: QueryMetricsOptions,
    ) -> Result<AggregatedMetrics, SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::QueryMetrics(options, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};

    use sozu_command_lib::proto::command::{
        Cluster, DeactivateListener, ListenerType, RequestHttpFrontend, TcpListenerConfig,
    };

    /// A failure message shaped like the real thing: the main process wraps
    /// `StateError::Exists`'s Display ("{kind:?} '{id}' already exists") in
    /// its own prefix.
    const EXISTS_MESSAGE: &str =
        "executing request on the state: HttpFrontend 'lb.example.com;/' already exists";

    fn socket_address(port: u16) -> sozu_command_lib::proto::command::SocketAddress {
        std::net::SocketAddr::from(([127, 0, 0, 1], port)).into()
    }

    /// A unique, short socket path (unix socket paths are length-limited).
    fn temp_socket_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sozu-gw-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A fake Sōzu: accepts one connection on `path` and answers every request
    /// with the given terminal failure message, through the crate's own
    /// `Channel` so the length-prefixed prost framing is the real one.
    fn spawn_fake_sozu_failing_with(path: &Path, message: &str) -> thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind fake sozu");
        let message = message.to_string();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // `mio` streams are expected to be nonblocking; `Channel::blocking`
            // flips the mode for the synchronous request/reply protocol.
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            // Serve until the client hangs up.
            while channel
                .read_message_blocking_timeout(Some(Duration::from_secs(5)))
                .is_ok()
            {
                channel
                    .write_message(&Response {
                        status: ResponseStatus::Failure as i32,
                        message: message.clone(),
                        content: None,
                    })
                    .expect("write response");
            }
        })
    }

    fn ok_response() -> Response {
        Response {
            status: ResponseStatus::Ok as i32,
            message: String::new(),
            content: None,
        }
    }

    fn failure_response(message: &str) -> Response {
        Response {
            status: ResponseStatus::Failure as i32,
            message: message.to_string(),
            content: None,
        }
    }

    /// A scripted fake Sōzu: accepts one connection on `path`, records every
    /// request it receives and answers each with the next response in `script`
    /// (failing loudly if the script runs dry). Returns the recorded requests
    /// when the client hangs up, so tests can assert the exact wire sequence.
    fn spawn_scripted_fake_sozu(
        path: &Path,
        script: Vec<Response>,
    ) -> thread::JoinHandle<Vec<Request>> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind fake sozu");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            let mut script = script.into_iter();
            let mut received = Vec::new();
            while let Ok(request) =
                channel.read_message_blocking_timeout(Some(Duration::from_secs(5)))
            {
                received.push(request);
                let response = script
                    .next()
                    .unwrap_or_else(|| failure_response("fake sozu script exhausted"));
                channel.write_message(&response).expect("write response");
            }
            received
        })
    }

    #[test]
    fn empty_batch_does_not_connect() {
        // No socket exists; an empty batch must succeed without touching it.
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        assert!(agent.apply(&[]).is_ok());
        assert!(agent.channel.is_none());
    }

    #[test]
    fn teardowns_are_recognized_for_failure_tolerance() {
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        assert!(is_teardown(&remove), "RemoveCluster must be a teardown");
        let deactivate: Request = RequestType::DeactivateListener(DeactivateListener {
            address: socket_address(7777),
            proxy: ListenerType::Tcp as i32,
            to_scm: false,
        })
        .into();
        assert!(
            is_teardown(&deactivate),
            "DeactivateListener must be a teardown (L4 listener teardown against a drifted Sōzu)"
        );
        let status: Request = RequestType::Status(Status {}).into();
        assert!(
            !is_teardown(&status),
            "non-teardown verbs must not be tolerated"
        );
    }

    #[test]
    fn duplicate_add_detection_is_scoped_to_exists_prone_verbs() {
        let add_frontend: Request =
            RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        assert!(
            is_duplicate_add(&add_frontend, EXISTS_MESSAGE),
            "a frontend add rejected as already-existing must get duplicate handling"
        );
        assert!(
            is_duplicate_add(&add_frontend, "HTTPFRONTEND 'X' ALREADY EXISTS"),
            "message matching must be case-insensitive"
        );
        assert!(
            !is_duplicate_add(&add_frontend, "wrong hostname"),
            "an unrelated add failure must still fail the batch"
        );

        let add_listener: Request =
            RequestType::AddTcpListener(TcpListenerConfig::default()).into();
        assert!(
            is_duplicate_add(&add_listener, "TcpListener '127.0.0.1:7777' already exists"),
            "an L4 listener add rejected as already-existing must get duplicate handling"
        );

        // AddCluster/AddBackend are upserts in Sōzu: an exists-looking failure
        // on them is unexpected and must not be swallowed.
        let add_cluster: Request = RequestType::AddCluster(Cluster::default()).into();
        assert!(
            !is_duplicate_add(&add_cluster, EXISTS_MESSAGE),
            "idempotent verbs must not get duplicate-add handling"
        );

        let remove: Request =
            RequestType::RemoveHttpFrontend(RequestHttpFrontend::default()).into();
        assert!(
            !is_duplicate_add(&remove, EXISTS_MESSAGE) && is_teardown(&remove),
            "removes stay on the teardown-tolerance path"
        );
    }

    #[test]
    fn removal_is_synthesized_only_for_frontend_adds() {
        let front = RequestHttpFrontend {
            cluster_id: Some("cluster-a".to_string()),
            hostname: "lb.example.com".to_string(),
            ..RequestHttpFrontend::default()
        };
        let add: Request = RequestType::AddHttpFrontend(front.clone()).into();
        let expected: Request = RequestType::RemoveHttpFrontend(front).into();
        assert_eq!(
            removal_for(&add),
            Some(expected),
            "a frontend add must repair via the remove built from its own payload"
        );

        let add_listener: Request =
            RequestType::AddTcpListener(TcpListenerConfig::default()).into();
        assert!(
            removal_for(&add_listener).is_none(),
            "listener adds are tolerated, never repaired (deactivate ordering)"
        );

        let add_cluster: Request = RequestType::AddCluster(Cluster::default()).into();
        assert!(
            removal_for(&add_cluster).is_none(),
            "upsert verbs never reach the repair path"
        );
    }

    /// The stored frontend behind an `Exists` may point at a *different*
    /// cluster (the route key excludes `cluster_id`/tags/filters), so the
    /// agent must not merely tolerate the failure: it must evict the stored
    /// entry and re-send its own add. Assert that exact wire sequence.
    #[test]
    fn duplicate_frontend_add_is_repaired_with_a_remove_and_a_readd() {
        let path = temp_socket_path("repair-add");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EXISTS_MESSAGE), // the add: duplicate route key
                ok_response(),                    // the repair remove
                ok_response(),                    // the re-sent add
            ],
        );

        let front = RequestHttpFrontend {
            cluster_id: Some("cluster-a".to_string()),
            hostname: "lb.example.com".to_string(),
            ..RequestHttpFrontend::default()
        };
        let add: Request = RequestType::AddHttpFrontend(front.clone()).into();

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(std::slice::from_ref(&add))
            .expect("a repaired duplicate frontend add must succeed");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        let expected = vec![
            add.clone(),
            RequestType::RemoveHttpFrontend(front).into(),
            add,
        ];
        assert_eq!(
            received, expected,
            "the repair must be a remove + re-add of the same payload"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// If the re-add after the repair fails hard, the batch must fail — the
    /// shadow stays put and the next reconcile retries; never a silent skip.
    #[test]
    fn failed_readd_after_repair_fails_the_batch() {
        let path = temp_socket_path("repair-fail");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EXISTS_MESSAGE),   // the add: duplicate route key
                ok_response(),                      // the repair remove
                failure_response("wrong hostname"), // the re-sent add: hard failure
            ],
        );

        let add: Request = RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(&[add]).unwrap_err();
        assert!(matches!(err, SozuError::Failure(_)), "got {err:?}");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received.len(),
            3,
            "the wire must show add, repair remove, failed re-add"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// L4 listener adds keep plain tolerance: they are keyed by address alone,
    /// so a duplicate cannot mask a different routing target — and no repair
    /// traffic may be emitted for them.
    #[test]
    fn duplicate_listener_add_is_tolerated_without_repair() {
        let path = temp_socket_path("dup-listener");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![failure_response(
                "TcpListener '127.0.0.1:7777' already exists",
            )],
        );

        let add: Request = RequestType::AddTcpListener(TcpListenerConfig {
            address: socket_address(7777),
            ..TcpListenerConfig::default()
        })
        .into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(&[add])
            .expect("a duplicate listener add must be tolerated so reconciliation converges");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 1, "no repair traffic for a listener add");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unrelated_add_failure_from_sozu_still_fails_the_batch() {
        let path = temp_socket_path("bad-add");
        let server = spawn_fake_sozu_failing_with(&path, "invalid frontend");

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let add: Request = RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        let err = agent.apply(&[add]).unwrap_err();
        assert!(matches!(err, SozuError::Failure(_)), "got {err:?}");

        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_to_missing_socket_is_channel_error() {
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        let err = agent.status().unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }

    #[test]
    fn query_metrics_to_missing_socket_is_channel_error() {
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        let err = agent
            .query_metrics(QueryMetricsOptions::default())
            .unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn handle_reports_connection_error() {
        let handle = SozuAgentHandle::spawn("/nonexistent/sozu.sock").expect("spawn");
        let err = handle
            .apply(vec![RequestType::Status(Status {}).into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }
}

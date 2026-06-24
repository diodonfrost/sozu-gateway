# PROTOCOL.md — Sōzu command socket, as actually observed

**Source of truth for the Translator and `sozu-agent`.** Everything here is taken
verbatim from the real crate source and **confirmed against a live Sōzu 2.1.0**
with the probe in [`crates/sozu-agent/examples/probe.rs`](crates/sozu-agent/examples/probe.rs).
If this document and the scoping prompt disagree, this document wins.

- **Crate**: `sozu-command-lib` **2.1.0** (latest on crates.io). License **LGPL-3.0**.
  Edition 2024, `rust-version = 1.88`. protobuf via **prost 0.14**.
- **Data plane**: Sōzu **2.1.0** (`clevercloud/sozu:2.1.0`; binary is **musl** → run via Docker).
- **Generated proto**: `src/proto/command.rs` (prost-generated; `package command;`, **`syntax = "proto2"`**).
- **Type paths**: there are *no* root re-exports. Use the literal paths:
  - `sozu_command_lib::proto::command::{Request, Response, Cluster, AddBackend, RemoveBackend, RequestHttpFrontend, AddCertificate, ReplaceCertificate, RemoveCertificate, CertificateAndKey, PathRule, SocketAddress, IpAddress, Status, ...}`
  - the request oneof: `sozu_command_lib::proto::command::request::RequestType`
  - enums: `...::command::{ResponseStatus, PathRuleKind, RulePosition, LoadBalancingAlgorithms, TlsVersion, ...}`
  - channel: `sozu_command_lib::channel::Channel`
  - cert helpers: `sozu_command_lib::certificate::{split_certificate_chain, get_cn_and_san_attributes, calculate_fingerprint, load_full_certificate}`
  - state: `sozu_command_lib::state::ConfigState`

---

## 1. Transport & wire format

The command socket is a **UNIX stream socket**. The crate's `Channel<Tx, Rx>` owns
the framing — **we never reimplement it**.

- Frame = **8-byte little-endian length prefix** (value = `payload_len + 8`) followed
  by the **prost-encoded** message. (Source: `channel.rs` write/read paths + raw-frame test.)
- The JSON + `\0` form some code paths use is **only for state files**
  (`SaveState`/`LoadState` on disk), **not** the socket.
- API (`channel.rs`):
  - `Channel::<Tx, Rx>::from_path(path: &str, buffer_size: u64, max_buffer_size: u64) -> Result<_, ChannelError>` — returns a **non-blocking** channel.
  - `channel.blocking()?` — switch to blocking for synchronous send/recv.
  - `channel.write_message(&Tx)?` / `channel.read_message() -> Result<Rx, ChannelError>`.
  - `Tx`/`Rx` must be `prost::Message + Default + Debug`.
- Buffer sizing: `command_buffer_size` / `max_command_buffer_size` in `config.toml`
  bound the *server* side; the client picks its own `from_path` sizes. We use 1 MiB /
  16 MiB (cert PEMs are small, but headroom is cheap).

### ✅ Transport type — RESOLVED by probe

The external client sends a **bare `Request`** — i.e. `Channel<Request, Response>`.
`WorkerRequest { id, content }` is the **Sōzu↔Sōzu internal** envelope (proto comment:
*"This is sent only from Sōzu to Sōzu"*) and is **not** used on the command socket.
Confirmed: `Status`, `AddCluster`, `AddBackend`, `AddHttpFrontend`, `AddCertificate`,
`AddHttpsFrontend` all succeeded sent as bare `Request`.

---

## 2. Request envelope & how to build one

```proto
message Request { oneof request_type { /* tagged variants */ } }
```

Build a request by wrapping a `RequestType` variant — `From<RequestType> for Request`
exists (`proto/mod.rs`), so just `.into()`:

```rust
use sozu_command_lib::proto::command::{request::RequestType, Request, Status};
let req: Request = RequestType::Status(Status {}).into();
```

proto2 enum fields are stored as **`i32`** in Rust; set them with `Variant as i32`.
There are **no** generated setter/getter accessors for these — assign the i32 directly.
prost `map<…>` fields become **`BTreeMap`** (deterministic). Every message derives
`serde::{Serialize, Deserialize}` (JSON enum case = `SCREAMING_SNAKE_CASE`).

---

## 3. `RequestType` variants used in Phase 1

Rust variant names (PascalCase; proto field names are snake_case):

| Variant | Payload | Use |
|---|---|---|
| `Status(Status)` | `Status {}` (empty) | health/ack probe |
| `AddCluster(Cluster)` | upsert | one cluster per target Service |
| `RemoveCluster(String)` | cluster_id | GC |
| `AddBackend(AddBackend)` | upsert | **pod IP:port** (never ClusterIP) |
| `RemoveBackend(RemoveBackend)` | | scale-down / pod churn |
| `AddHttpFrontend(RequestHttpFrontend)` | verb=15 | HTTP route |
| `RemoveHttpFrontend(RequestHttpFrontend)` | verb=16 | |
| `AddHttpsFrontend(RequestHttpFrontend)` | verb=17 | HTTPS route (**same payload**, different verb) |
| `RemoveHttpsFrontend(RequestHttpFrontend)` | verb=18 | |
| `AddCertificate(AddCertificate)` | | load Secret cert onto :443 listener |
| `ReplaceCertificate(ReplaceCertificate)` | | atomic cert rotation |
| `RemoveCertificate(RemoveCertificate)` | by fingerprint | |
| `LoadState(String)` / `SaveState(String)` | path | bootstrap/persist (optional) |
| `ListFrontends(FrontendFilters)` / `QueryClusterById(String)` | | verification/queries |
| `AddHttpListener`/`AddHttpsListener`/`ActivateListener` | | **not needed** when listeners are static in `config.toml` (see §8) |

> HTTP vs HTTPS is the **verb** (`AddHttpFrontend` vs `AddHttpsFrontend`), the payload
> type is identical (`RequestHttpFrontend`); the `address` field selects which listener.

---

## 4. Message field reference (exact, from `proto/command.rs`)

`required` proto2 fields are plain Rust types; `optional` → `Option<T>`; `repeated` → `Vec<T>`.
Use `..Default::default()` for everything not listed below.

### `RequestHttpFrontend`
```rust
cluster_id: Option<String>   // None = a deny/redirect-only frontend; Some(id) routes to a cluster
address:    SocketAddress    // REQUIRED — the listener address this attaches to (e.g. 0.0.0.0:443)
hostname:   String           // REQUIRED — SNI/Host match
path:       PathRule         // REQUIRED — see PathRule
method:     Option<String>
position:   i32 (RulePosition, default Tree)
tags:       BTreeMap<String,String>
// Phase-3 fields (leave default in Phase 1): redirect, required_auth, redirect_scheme,
// redirect_template, rewrite_host, rewrite_path, rewrite_port, headers, hsts, ...
```

### `Cluster`
```rust
cluster_id:     String        // REQUIRED
sticky_session: bool          // REQUIRED (Phase 1: false)
https_redirect: bool          // REQUIRED (Phase 1: false)
load_balancing: i32           // REQUIRED — LoadBalancingAlgorithms (RoundRobin = 0)
proxy_protocol: Option<i32>, answer_503: Option<String>, load_metric: Option<i32>, http2: Option<bool>
// Phase-3 fields: answers, https_redirect_port, authorized_hashes, www_authenticate,
// max_connections_per_ip, retry_after, health_check, udp ...
```

### `AddBackend` / `RemoveBackend`
```rust
// AddBackend
cluster_id: String           // REQUIRED
backend_id: String           // REQUIRED — stable id per endpoint, e.g. "<cluster>-<ip>-<port>"
address:    SocketAddress    // REQUIRED — the POD IP:port
sticky_id:  Option<String>, load_balancing_parameters: Option<LoadBalancingParams>, backup: Option<bool>
// RemoveBackend = { cluster_id, backend_id, address } (all required)
```

### `PathRule`
```rust
kind:  i32      // PathRuleKind: Prefix=0, Regex=1, Equals=2
value: String   // the prefix / regex / exact value
```

### `SocketAddress` / `IpAddress`
```rust
SocketAddress { ip: IpAddress, port: u32 }     // both REQUIRED
IpAddress { inner: Option<ip_address::Inner> } // Inner::V4(u32 fixed32) | V6(Uint128{low,high})
```
**Never hand-pack.** Use `From<std::net::SocketAddr>`:
`let a: SocketAddress = "10.0.0.5:8080".parse::<SocketAddr>()?.into();`
(also `SocketAddress::new_v4(a,b,c,d,port)`). ✅ Round-trip verified — traffic reached
`127.0.0.1:9000` correctly, so v4 `fixed32` packing is correct end-to-end.

### `AddCertificate` / `ReplaceCertificate` / `RemoveCertificate` / `CertificateAndKey`
```rust
AddCertificate     { address: SocketAddress, certificate: CertificateAndKey, expired_at: Option<i64> }
ReplaceCertificate { address, new_certificate: CertificateAndKey, old_fingerprint: String, new_expired_at: Option<i64> }
RemoveCertificate  { address: SocketAddress, fingerprint: String }   // hex-encoded fingerprint
CertificateAndKey {
    certificate:       String,        // REQUIRED — leaf PEM
    certificate_chain: Vec<String>,   // intermediate PEMs (empty for self-signed)
    key:               String,        // REQUIRED — private key PEM
    versions:          Vec<i32>,      // TlsVersion; empty => server default (TLS 1.2 + 1.3)
    names:             Vec<String>,   // explicit SNI names; if empty, derived from cert
}
```
`address` binds the cert to the matching **listener address** (`0.0.0.0:443`); SNI
selection is then done by Sōzu. ✅ Verified: explicit `names = [host]` served the right
cert under SNI (`openssl s_client -servername app.example.com` returned our CN/SAN).

---

## 5. Enums (exact i32 values)

```text
ResponseStatus:           Ok=0, Processing=1, Failure=2
PathRuleKind:             Prefix=0, Regex=1, Equals=2
RulePosition:             Pre=0, Post=1, Tree=2
LoadBalancingAlgorithms:  RoundRobin=0, Random=1, LeastLoaded=2, PowerOfTwo=3, Hrw=4, Maglev=5
TlsVersion:               SslV2=0, SslV3=1, TlsV10=2, TlsV11=3, TlsV12=4, TlsV13=5
                          (JSON / config.toml spelling: "TLS_V12", "TLS_V13")
```

---

## 6. Response semantics — ✅ verified

`Response { status: i32 (ResponseStatus), message: String, content: Option<ResponseContent> }`.

- **Every** request returns an **interim `Processing`** message **then** a terminal
  `Ok`/`Failure`. The client **must loop reading until `status != Processing`.**
  Observed terminal messages: apply ops → `"Successfully applied request to all workers"`;
  `Status` → `"Successfully collected the status of workers"`.
- For broadcast apply ops, the terminal `status == Ok` means *all workers applied*.
  `content` was `None` for `Add*`; `Status` carried `ContentType::Workers(WorkerInfos)`.
- `content_type` oneof of interest: `Workers`, `WorkerResponses(map<id, …>)`,
  `FrontendList(ListedFrontends)`, `Clusters(ClusterInformations)`, `ListenersList`.
  (When `content` is `WorkerResponses`, individual sub-responses *could* differ from the
  aggregate — for >1 worker we should walk the map; not yet observed with 1 worker.)

---

## 7. Certificates

Build `CertificateAndKey` straight from the Kubernetes Secret bytes (`tls.crt` / `tls.key`):

- `split_certificate_chain(pem: String) -> Vec<String>` splits a concatenated PEM into
  individual certs (leaf is element 0; remainder is the chain).
- `get_cn_and_san_attributes(&X509Certificate) -> Vec<String>` derives SNI names if we
  ever want to (we set `names` explicitly from the Ingress host instead).
- `calculate_fingerprint(pem_bytes) -> Vec<u8>` for the hex fingerprint used by
  `RemoveCertificate`/`ReplaceCertificate`.
- **Rotation decision**: `ConfigState::diff` emits cert change as `RemoveCertificate(old)` +
  `AddCertificate(new)` (brief gap). For zero-gap rotation we will emit
  `ReplaceCertificate { old_fingerprint, new_certificate }` ourselves.

---

## 8. Listeners — static in `config.toml` (decided & verified)

✅ With `activate_listeners = true` and `[[listeners]]` for `:80` (http) and `:443`
(https) in `config.toml`, **`AddHttpFrontend`/`AddHttpsFrontend`/`AddCertificate`
succeed without any `AddHttpListener`/`ActivateListener`** over the socket.

**Design**: the Helm chart ships listeners in `config.toml`; the controller manages
only clusters / frontends / backends / certificates. (Dynamic listener creation over
the socket remains available if we ever need per-listener tuning at runtime.)
Note: HTTPS listeners advertising `h2` require `buffer_size >= 16393`.

---

## 9. `ConfigState` & the diff strategy — REUSE sozu's diff

`sozu_command_lib::state::ConfigState`:
```rust
ConfigState::new() -> Self
fn dispatch(&mut self, request: &Request) -> Result<(), StateError>   // fold an Add*/Remove* into state
fn diff(&self, other: &ConfigState) -> Vec<Request>                   // minimal requests: self -> other
fn generate_activate_requests(&self) -> Vec<Request>
fn produce_initial_state(&self) -> InitialState
fn cluster_state(&self, id: &str) -> Option<ClusterInformation>
```

**Translator plan**: build the desired `ConfigState` by folding our `Add*` requests via
`dispatch` into a fresh `ConfigState`; keep a **shadow** `ConfigState` of last-applied;
emit `shadow.diff(&desired)`; on success set `shadow = desired`. This is idempotent
(`s.diff(&s)` is empty) and the crate self-verifies `diff` in its own debug-assert tests.

Caveats (respect these):
- `generate_requests` is **private**; `diff` / `dispatch` / `produce_initial_state` /
  `generate_activate_requests` are the public entry points.
- Frontend/certificate ordering in `diff` output is **non-deterministic** (HashSet diff);
  clusters/backends/listeners are deterministic (BTreeMap). **Sort the `Vec<Request>`** if
  we want stable logs/golden tests.
- We still keep an in-memory shadow as the "last applied"; on (re)start we push the full
  desired state (and may `LoadState`/`SaveState` to a file for crash recovery — TBD).

---

## 10. Verified end-to-end sequence (probe output)

```
[1] Status                 -> PROCESSING -> OK  (content: Workers[id=0, Running])
[2] AddCluster             -> PROCESSING -> OK  "Successfully applied request to all workers"
    AddBackend             -> PROCESSING -> OK
    AddHttpFrontend        -> PROCESSING -> OK
[3] AddCertificate         -> PROCESSING -> OK
    AddHttpsFrontend       -> PROCESSING -> OK
[4] Status (idempotent)    -> PROCESSING -> OK

curl http  app.example.com:8080  -> 200   (served by backend through Sōzu)
curl https app.example.com:8443  -> 200
openssl s_client -servername app.example.com -> subject CN=app.example.com, SAN app.example.com
```

Reproduce: `bash .scratch/run-probe.sh` (will be promoted into the `justfile`).

---

## 11. Kubernetes Ingress → Sōzu mapping (Phase 1)

| K8s | Sōzu |
|---|---|
| Ingress rule `host` (exact) | `RequestHttpFrontend.hostname` |
| Ingress rule `host` wildcard `*.x` | `hostname` wildcard (Sōzu supports `*.` prefix) — to confirm in Étape 2 |
| `pathType: Prefix` | `PathRule { Prefix, value }` |
| `pathType: Exact` | `PathRule { Equals, value }` |
| `pathType: ImplementationSpecific` | `PathRule { Regex, value }` (⚠ 2.x anchors regexes — Étape 2) |
| backend `Service` → `EndpointSlice` pod IPs | one `Cluster` + N `AddBackend` (pod `IP:port`) |
| `spec.tls[].secretName` (`tls.crt`/`tls.key`) | `AddCertificate` on the `:443` listener address |
| HTTP rule | `AddHttpFrontend` on `:80` listener address |
| TLS host | `AddHttpsFrontend` on `:443` listener address |

---

## 12. Open questions / decisions (see chat — pending your input)

1. **Listener ports / model**: static listeners on `:80`+`:443` in `config.toml`
   (recommended, verified) vs dynamic via socket. → defaulting to static.
2. **State persistence on restart**: rely solely on the in-memory shadow + full re-push at
   startup, or also `SaveState`/`LoadState` a file for fast crash recovery? → defaulting to
   re-push; file persistence optional later.
3. **Wildcard host + regex path semantics** in Sōzu 2.x need a dedicated micro-probe in
   Étape 2 before we finalize the Builder's path mapping (regex anchoring caveat).
4. **Multi-worker fan-in**: with `worker_count > 1`, confirm whether to inspect per-worker
   `WorkerResponses` for partial failure (we'll add defensive handling in `sozu-agent`).

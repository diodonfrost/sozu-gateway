//! Render Sōzu `AggregatedMetrics` as Prometheus text exposition format (v0.0.4).
//!
//! Pure and I/O-free: it only borrows the protobuf types from `sozu-command-lib`,
//! mirroring the `translator` crate's purity. The controller calls [`render`]
//! with the `AggregatedMetrics` it pulls over the command socket (a
//! `QueryMetrics` request) and serves the result at `/metrics`. Sōzu has no
//! native `/metrics` endpoint, but its metric model is Prometheus-shaped.
//!
//! Mapping (every name prefixed `sozu_`; bytes outside `[A-Za-z0-9_]` → `_`):
//! - `Gauge`       → gauge
//! - `Count`       → counter (kept faithful to Sōzu's name — no forced `_total`)
//! - `Histogram`   → histogram. Sōzu's bucket counts are already **cumulative**
//!   (count of observations ≤ `le`; the data plane stores them that way — see
//!   `print_histograms` in sozu-command-lib, which subtracts the previous bucket
//!   to recover per-bucket counts), so they map straight onto Prometheus
//!   `_bucket{le}`, with an added `+Inf` bucket equal to the total count.
//! - `Percentiles` → summary (one series per quantile, plus `_sum`/`_count`).
//!   ⚠ Sōzu cannot statistically merge percentiles across workers (it takes the
//!   element-wise max); the companion `*_histogram` is the accurate source.
//! - `Time` / `TimeSerie` → skipped (never emitted by Sōzu).
//!
//! Label sets: proxy metrics have none; main-process metrics carry
//! `process="main"` (so they never collide with an identically-named proxy
//! series); cluster metrics carry `cluster_id`; backend metrics carry
//! `cluster_id` + `backend_id`.
//!
//! Two intentional drops, so the exposition stays spec-valid:
//! - `AggregatedMetrics.workers` is **not rendered**. The controller queries
//!   with workers pre-merged (Sōzu folds them into the proxy/cluster maps), so
//!   the field is empty on the production path; a caller passing worker-scoped
//!   data must know it is dropped, never silently mixed into the merged series.
//! - A series whose kind conflicts with its family's already-established type
//!   (same sanitized name, different metric kind) is **skipped**: rendering,
//!   say, summary-shaped `quantile`/`_sum`/`_count` lines under a `# TYPE ...
//!   counter` is invalid exposition that Prometheus may reject wholesale. The
//!   drop is surfaced as a `sozu_gw_dropped_series` gauge on the scrape itself.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sozu_command_lib::proto::command::{
    filtered_metrics::Inner, AggregatedMetrics, FilteredHistogram, FilteredMetrics, Percentiles,
};

/// One Prometheus metric family: its type plus every series (one per label set,
/// across proxy / main / cluster / backend) sharing the same base name.
struct Family {
    mtype: &'static str,
    /// `(sort_key, rendered_lines)` per series — sorted on output for
    /// deterministic exposition (golden-snapshot friendly).
    series: Vec<(String, String)>,
}

/// The families under construction, plus the count of series skipped because
/// their kind conflicted with their family's established type.
#[derive(Default)]
struct Families {
    map: BTreeMap<String, Family>,
    dropped: usize,
}

impl Families {
    /// Attach a rendered series to its family. The first kind seen under a
    /// name fixes the family's `# TYPE`; a later series of a *different* kind
    /// is differently shaped (`quantile`/`_bucket`/`_sum` lines) and rendering
    /// it under that TYPE would be spec-invalid exposition — it is skipped and
    /// counted instead.
    fn insert(&mut self, base: String, mtype: &'static str, sort_key: String, lines: String) {
        let family = self.map.entry(base).or_insert_with(|| Family {
            mtype,
            series: Vec::new(),
        });
        if family.mtype != mtype {
            self.dropped += 1;
            return;
        }
        family.series.push((sort_key, lines));
    }
}

/// Render `metrics` as a Prometheus text-format exposition document.
///
/// `metrics.workers` is intentionally ignored, and a series whose kind
/// conflicts with its family's established type is skipped (see the module
/// docs); skipped series are reported in-band as a `sozu_gw_dropped_series`
/// gauge, rendered only when non-zero.
pub fn render(metrics: &AggregatedMetrics) -> String {
    let mut families = Families::default();

    // Proxy metrics, already merged across workers — no labels.
    for (name, fm) in &metrics.proxying {
        add_metric(&mut families, name, &[], fm);
    }
    // Main (master) process metrics — tagged so they never alias a proxy series.
    for (name, fm) in &metrics.main {
        add_metric(&mut families, name, &[("process", "main")], fm);
    }
    // Per-cluster and per-backend metrics.
    for (cluster_id, cm) in &metrics.clusters {
        for (name, fm) in &cm.cluster {
            add_metric(&mut families, name, &[("cluster_id", cluster_id)], fm);
        }
        for backend in &cm.backends {
            let labels = [
                ("cluster_id", cluster_id.as_str()),
                ("backend_id", backend.backend_id.as_str()),
            ];
            for (name, fm) in &backend.metrics {
                add_metric(&mut families, name, &labels, fm);
            }
        }
    }

    let mut out = String::new();
    for (base, mut family) in families.map {
        let _ = writeln!(out, "# HELP {base} Sozu data-plane metric.");
        let _ = writeln!(out, "# TYPE {base} {}", family.mtype);
        family.series.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, lines) in family.series {
            out.push_str(&lines);
        }
    }
    // The crate is pure (no logging), so the drop is reported in-band, on the
    // scrape it happened in — visible to whoever reads the exposition.
    if families.dropped > 0 {
        let _ = writeln!(
            out,
            "# HELP sozu_gw_dropped_series Series dropped from this exposition because their metric kind conflicted with their family's established type."
        );
        let _ = writeln!(out, "# TYPE sozu_gw_dropped_series gauge");
        let _ = writeln!(out, "sozu_gw_dropped_series {}", families.dropped);
    }
    out
}

fn add_metric(
    families: &mut Families,
    raw_name: &str,
    labels: &[(&str, &str)],
    fm: &FilteredMetrics,
) {
    let base = sanitize(raw_name);
    match &fm.inner {
        Some(Inner::Gauge(v)) => push_scalar(families, base, "gauge", labels, &v.to_string()),
        Some(Inner::Count(v)) => push_scalar(families, base, "counter", labels, &v.to_string()),
        Some(Inner::Histogram(h)) => push_histogram(families, base, labels, h),
        Some(Inner::Percentiles(p)) => push_summary(families, base, labels, p),
        // Time / TimeSerie are never emitted by Sōzu; None carries nothing.
        Some(Inner::Time(_)) | Some(Inner::TimeSerie(_)) | None => {}
    }
}

/// `sozu_` followed by `raw` with every non-`[A-Za-z0-9_]` char turned into `_`.
fn sanitize(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len() + 5);
    s.push_str("sozu_");
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    s
}

fn push_scalar(
    families: &mut Families,
    base: String,
    mtype: &'static str,
    labels: &[(&str, &str)],
    value: &str,
) {
    let lset = fmt_labels(labels, &[]);
    let line = format!("{base}{lset} {value}\n");
    families.insert(base, mtype, lset, line);
}

fn push_histogram(
    families: &mut Families,
    base: String,
    labels: &[(&str, &str)],
    h: &FilteredHistogram,
) {
    let mut lines = String::new();
    // Sōzu bucket counts are already cumulative, so they are valid Prometheus
    // `le` buckets verbatim.
    for b in &h.buckets {
        let le = b.le.to_string();
        let lset = fmt_labels(labels, &[("le", &le)]);
        let _ = writeln!(lines, "{base}_bucket{lset} {}", b.count);
    }
    // The mandatory +Inf bucket equals the total observation count.
    let inf = fmt_labels(labels, &[("le", "+Inf")]);
    let _ = writeln!(lines, "{base}_bucket{inf} {}", h.count);
    let lset = fmt_labels(labels, &[]);
    let _ = writeln!(lines, "{base}_sum{lset} {}", h.sum);
    let _ = writeln!(lines, "{base}_count{lset} {}", h.count);
    families.insert(base, "histogram", lset, lines);
}

fn push_summary(families: &mut Families, base: String, labels: &[(&str, &str)], p: &Percentiles) {
    let mut lines = String::new();
    for (quantile, value) in [
        ("0.5", p.p_50),
        ("0.9", p.p_90),
        ("0.99", p.p_99),
        ("0.999", p.p_99_9),
        ("0.9999", p.p_99_99),
        ("0.99999", p.p_99_999),
        ("1", p.p_100),
    ] {
        let lset = fmt_labels(labels, &[("quantile", quantile)]);
        let _ = writeln!(lines, "{base}{lset} {value}");
    }
    let lset = fmt_labels(labels, &[]);
    let _ = writeln!(lines, "{base}_sum{lset} {}", p.sum);
    let _ = writeln!(lines, "{base}_count{lset} {}", p.samples);
    families.insert(base, "summary", lset, lines);
}

/// Format a label set as `{k1="v1",k2="v2"}` (empty string when no labels).
/// `extra` is appended after `base` — used for the synthetic `le` / `quantile`.
fn fmt_labels(base: &[(&str, &str)], extra: &[(&str, &str)]) -> String {
    if base.is_empty() && extra.is_empty() {
        return String::new();
    }
    let mut s = String::from("{");
    for (i, (k, v)) in base.iter().chain(extra.iter()).enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{k}=\"{}\"", escape(v));
    }
    s.push('}');
    s
}

/// Escape a label value per the Prometheus text format (`\`, `"`, newline).
fn escape(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            _ => s.push(c),
        }
    }
    s
}

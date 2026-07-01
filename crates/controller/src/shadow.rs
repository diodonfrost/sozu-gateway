//! Shadow persistence — survive a controller-only restart without losing the
//! ability to prune orphaned Sōzu state.
//!
//! The shadow (last-applied IR) lives in memory and normally resets to empty
//! when the controller process restarts. Both containers share an `emptyDir`, so
//! if *only* the controller restarts, Sōzu keeps its live state but the
//! controller would re-add everything from an empty baseline and never compute
//! the removes for objects deleted meanwhile.
//!
//! So we persist the shadow to that shared volume and reload it on startup — but
//! only when Sōzu *still holds the state it describes*. If Sōzu itself restarted
//! (empty), the persisted shadow is stale and trusting it would leave a fresh
//! Sōzu unprogrammed; in that case we start empty and re-apply everything. Any
//! error falls back to empty too, because re-applying is always correct.

use std::collections::BTreeSet;

use anyhow::Context;
use sozu_gw_agent::{SozuAgentHandle, SozuError};
use sozu_gw_ir::Ir;
use tracing::{debug, info, warn};

/// Load the initial shadow. Returns the persisted last-applied IR only when it
/// is safe to trust (file present AND Sōzu non-empty); otherwise an empty IR.
pub async fn load_initial(agent: &SozuAgentHandle, shadow_file: &str, probe_file: &str) -> Ir {
    if shadow_file.is_empty() {
        return Ir::default();
    }
    let raw = match std::fs::read_to_string(shadow_file) {
        Ok(s) => s,
        Err(e) => {
            debug!(error = %e, file = %shadow_file, "no persisted shadow; starting empty");
            return Ir::default();
        }
    };
    // The persisted shadow is only trustworthy if Sōzu still has its state.
    match sozu_has_state(agent, probe_file).await {
        Ok(true) => {}
        Ok(false) => {
            info!("Sōzu state is empty (restarted?); ignoring persisted shadow, will re-apply");
            return Ir::default();
        }
        Err(e) => {
            warn!(error = %e, "could not probe Sōzu state; ignoring persisted shadow, will re-apply");
            return Ir::default();
        }
    }
    match serde_json::from_str::<Ir>(&raw) {
        Ok(ir) => {
            info!(file = %shadow_file, "resumed shadow from persisted state");
            ir
        }
        Err(e) => {
            warn!(error = %e, "persisted shadow is unreadable; will re-apply");
            Ir::default()
        }
    }
}

/// Outcome of a restart-generation check, for the caller's control flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationCheck {
    /// The probe succeeded and the generation matches the baseline (or there
    /// was nothing applied to lose); the shadow stands.
    Unchanged,
    /// The probe succeeded, the generation changed under a non-empty shadow,
    /// and the shadow was reset — a full re-apply is due.
    Reset,
    /// The probe failed; nothing was decided. The caller must retry: never
    /// reset (a blind full re-apply) and never conclude "no restart" from an
    /// error.
    ProbeFailed,
}

/// Mid-life counterpart of [`load_initial`]: check Sōzu's *restart generation*
/// — its live worker-PID set — against the baseline, and reset the shadow to
/// empty when it changed.
///
/// If the Sōzu container restarts under a live controller (main-process crash;
/// `worker_automatic_restart` only covers workers), it comes back empty while
/// the in-memory shadow still claims everything is applied — the diff stays
/// empty and every request 404s indefinitely. An emptiness probe cannot detect
/// this reliably: any successful add-bearing apply that lands on the restarted
/// Sōzu first (e.g. the tail of the very batch whose reconnect signalled the
/// restart) makes it non-empty again, masking the restart forever. The
/// worker-PID set is immune to that race — the restarted main process forks
/// fresh workers no matter what got re-applied. The cost is a false positive
/// on a single worker bounce (`worker_automatic_restart` changes one PID): an
/// acceptable, logged, harmless full re-apply.
///
/// On success the baseline advances to the observed set. A missing baseline
/// (the startup capture failed) resets too when the shadow is non-empty: with
/// no established generation there is no proof Sōzu still holds what the
/// shadow claims, and one extra full re-apply is the safe way out.
pub async fn check_restart_generation(
    agent: &SozuAgentHandle,
    baseline: &mut Option<BTreeSet<i32>>,
    shadow: &mut Ir,
) -> GenerationCheck {
    let probe = agent.worker_pids().await;
    if let Err(e) = &probe {
        warn!(error = %e, "could not query Sōzu's workers; keeping the shadow and retrying");
        return GenerationCheck::ProbeFailed;
    }
    let outcome = if should_reset(&probe, baseline.as_ref(), shadow) {
        warn!(
            baseline = ?baseline,
            current = ?probe.as_ref().ok(),
            "Sōzu's worker generation changed (restarted?); resetting the shadow to re-apply the full state"
        );
        *shadow = Ir::default();
        GenerationCheck::Reset
    } else {
        GenerationCheck::Unchanged
    };
    if let Ok(pids) = probe {
        *baseline = Some(pids);
    }
    outcome
}

/// Pure reset decision, keyed on (probe result, PID-set change, shadow
/// emptiness). A probe *error* never resets (a transient failure must not
/// trigger a full blind re-apply — the caller retries), and an empty shadow
/// never resets (nothing applied, nothing to lose). On a successful probe the
/// shadow is reset when the PID set differs from the baseline — including a
/// single worker bounce — or when no baseline was ever established (an
/// unproven generation under a claimed-applied shadow is not trustworthy).
fn should_reset(
    probe: &Result<BTreeSet<i32>, SozuError>,
    baseline: Option<&BTreeSet<i32>>,
    shadow: &Ir,
) -> bool {
    let Ok(pids) = probe else {
        return false;
    };
    if *shadow == Ir::default() {
        return false;
    }
    match baseline {
        Some(known) => known != pids,
        None => true,
    }
}

/// Probe whether Sōzu currently holds any routing state, by asking it to dump to
/// `probe_file` (on the shared volume) and checking the dump is non-empty. A
/// dump-file read failure is an *error*, never "empty": conflating the two
/// would let a controller-side filesystem hiccup read as a restarted Sōzu.
async fn sozu_has_state(agent: &SozuAgentHandle, probe_file: &str) -> anyhow::Result<bool> {
    agent
        .save_state(probe_file.to_string())
        .await
        .context("ask Sōzu to dump its state")?;
    let dump = read_state_dump(probe_file)
        .with_context(|| format!("read Sōzu's state dump at {probe_file}"))?;
    Ok(state_dump_is_nonempty(&dump))
}

/// Read (and best-effort clean up) the probe dump. Errors surface to the
/// caller — see [`sozu_has_state`].
fn read_state_dump(probe_file: &str) -> std::io::Result<String> {
    let dump = std::fs::read_to_string(probe_file)?;
    let _ = std::fs::remove_file(probe_file); // best-effort cleanup
    Ok(dump)
}

/// A Sōzu state dump is newline/NUL-delimited JSON records; it is non-empty when
/// it has at least one record.
fn state_dump_is_nonempty(dump: &str) -> bool {
    dump.split('\n')
        .any(|line| !line.trim_matches(['\0', ' ', '\r', '\t']).is_empty())
}

/// Persist the shadow. Best-effort: a write failure must never fail a reconcile.
pub fn persist(shadow_file: &str, shadow: &Ir) {
    if shadow_file.is_empty() {
        return;
    }
    match serde_json::to_vec(shadow) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(shadow_file, bytes) {
                warn!(error = %e, file = %shadow_file, "failed to persist shadow");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize shadow"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dump_is_detected_as_empty() {
        assert!(!state_dump_is_nonempty(""));
        assert!(!state_dump_is_nonempty("\n\0\n"));
        assert!(!state_dump_is_nonempty("   \r\n"));
    }

    #[test]
    fn nonempty_dump_is_detected() {
        assert!(state_dump_is_nonempty(
            "{\"id\":\"SAVE-0\",\"content\":{}}\n\0"
        ));
    }

    #[test]
    fn reset_only_on_a_successful_probe_showing_a_new_generation() {
        let applied = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::default(),
                sticky_session: false,
                https_redirect: false,
                max_connections_per_ip: None,
                retry_after: None,
            }],
            ..Default::default()
        };
        let empty = Ir::default();
        let baseline = BTreeSet::from([101, 102]);

        // Sōzu's main process restarted: every worker PID is new.
        assert!(should_reset(
            &Ok(BTreeSet::from([201, 202])),
            Some(&baseline),
            &applied
        ));
        // A single worker bounce (worker_automatic_restart) resets too: an
        // acceptable, logged, harmless full re-apply.
        assert!(should_reset(
            &Ok(BTreeSet::from([101, 103])),
            Some(&baseline),
            &applied
        ));
        // Same set: Sōzu did not restart, the shadow stands.
        assert!(!should_reset(
            &Ok(baseline.clone()),
            Some(&baseline),
            &applied
        ));
        // Nothing was ever applied: nothing a restarted Sōzu could have lost.
        assert!(!should_reset(
            &Ok(BTreeSet::from([201, 202])),
            Some(&baseline),
            &empty
        ));
        // A probe error must never trigger a full blind re-apply — the caller
        // keeps the reconnect pending and retries.
        assert!(!should_reset(
            &Err(sozu_gw_agent::SozuError::WorkerGone),
            Some(&baseline),
            &applied
        ));
        // No baseline was ever captured while the shadow claims applied state:
        // the generation is unproven, so reset (one extra full re-apply).
        assert!(should_reset(&Ok(baseline.clone()), None, &applied));
        // ... but an empty shadow with no baseline is just a fresh start.
        assert!(!should_reset(&Ok(baseline), None, &empty));
    }

    #[test]
    fn a_missing_state_dump_is_an_error_not_an_empty_dump() {
        // A controller-side read failure must surface as an error (the caller
        // keeps/ignores the shadow accordingly), never read as "Sōzu is empty".
        let missing =
            std::env::temp_dir().join(format!("sozu-gw-nonexistent-{}.probe", std::process::id()));
        assert!(read_state_dump(missing.to_str().expect("utf-8 path")).is_err());
    }

    #[test]
    fn shadow_round_trips_through_json() {
        // A representative IR must survive serialize -> deserialize unchanged, so
        // a resumed shadow diffs cleanly against a freshly-built desired IR.
        let ir = Ir {
            clusters: vec![sozu_gw_ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: sozu_gw_ir::LbAlgorithm::LeastLoaded,
                sticky_session: true,
                https_redirect: false,
                max_connections_per_ip: Some(100),
                retry_after: Some(5),
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&ir).unwrap();
        let back: Ir = serde_json::from_str(&json).unwrap();
        assert_eq!(ir, back);
    }
}

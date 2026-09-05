//! Draining captured agent session ids onto the rows they belong to.

use crate::session::Instance;
use std::sync::Arc;

use super::state::AppState;

pub(super) type SessionIdentityBaseline = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<std::time::Instant>,
    Option<std::time::SystemTime>,
    u64,
    crate::session::Status,
);

/// Merge a drained instance's captured identity back into live state, but only
/// the identity fields and only if they are unchanged since the baseline. The
/// daemon needs this field-level, baseline-guarded merge because it drops the
/// shared async lock across `spawn_blocking`, so the live instance may have
/// been mutated meanwhile. The single-threaded TUI re-inserts the whole
/// instance instead (see `apply_session_id_updates`); keep the two in sync.
pub(super) fn apply_drained_identity_if_unchanged(
    live: &mut Instance,
    drained: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    let (baseline_sid, baseline_marker, baseline_generation, _, _, _, _) = baseline;
    if live.agent_session_id == *baseline_sid && live.omp_capture_generation == *baseline_generation
    {
        live.agent_session_id = drained.agent_session_id.clone();
        live.omp_capture_generation = drained.omp_capture_generation.clone();
        if live.resume_probe_failed_sid == *baseline_marker {
            live.resume_probe_failed_sid = drained.resume_probe_failed_sid.clone();
        }
    }
}

fn apply_poller_runtime_if_unchanged(
    live: &mut Instance,
    repaired: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    if live.omp_capture_generation == repaired.omp_capture_generation
        && live.session_id_poller_retry_after == baseline.3
        && live.capture_started_at == baseline.4
        && live.lifecycle_generation == baseline.5
        && live.status == baseline.6
        && !live.session_id_poller_is_running()
    {
        live.session_id_poller_retry_after = repaired.session_id_poller_retry_after;
        if repaired.session_id_poller_is_running() {
            live.session_id_poller = repaired.session_id_poller.clone();
        }
    }
}

pub(super) async fn drain_session_id_updates_in_state(state: &Arc<AppState>) {
    // Drain poller observations into sessions.json so daemon-only sessions
    // persist post-`/clear` sids (#2291). Snapshot + spawn_blocking + reapply,
    // never holding AppState across the flock or tmux exec, per storage.rs:46.
    let snapshot = state.instances.read().await.clone();
    let file_watch = state.file_watch.clone();
    match tokio::task::spawn_blocking(move || {
        let baseline: std::collections::HashMap<String, SessionIdentityBaseline> = snapshot
            .iter()
            .map(|inst| {
                (
                    inst.id.clone(),
                    (
                        inst.agent_session_id.clone(),
                        inst.resume_probe_failed_sid.clone(),
                        inst.omp_capture_generation.clone(),
                        inst.session_id_poller_retry_after,
                        inst.capture_started_at,
                        inst.lifecycle_generation,
                        inst.status,
                    ),
                )
            })
            .collect();
        let mut snapshot = snapshot;
        // Preserve a final queued observation before replacing a stopped
        // worker. Repair runs afterward and binds to any generation the drain
        // just made durable.
        let outcome =
            crate::session::sync::drain_and_persist_session_ids(&mut snapshot, &file_watch);
        // One observation for the whole repair walk, as on the TUI side: this
        // visits every instance, so a per-item `list-sessions` fork scales with
        // the store.
        let live = crate::tmux::LiveSessionSnapshot::new();
        let runtime_changed: std::collections::HashSet<String> = snapshot
            .iter_mut()
            .filter_map(|inst| {
                let retry_before = inst.session_id_poller_retry_after;
                let started = inst.repair_session_id_poller_if_needed(&live);
                (started || inst.session_id_poller_retry_after != retry_before)
                    .then(|| inst.id.clone())
            })
            .collect();
        (outcome, snapshot, baseline, runtime_changed)
    })
    .await
    {
        Ok((outcome, mutated, baseline, runtime_changed))
            if outcome.touched() || !runtime_changed.is_empty() =>
        {
            let touched: std::collections::HashSet<&str> = outcome
                .applied
                .iter()
                .chain(outcome.rolled_back.iter())
                .map(String::as_str)
                .collect();
            let mut guard = state.instances.write().await;
            for src in &mutated {
                let Some(dst) = guard.iter_mut().find(|i| i.id == src.id) else {
                    continue;
                };
                let Some(identity_baseline) = baseline.get(&src.id) else {
                    continue;
                };
                if touched.contains(src.id.as_str()) {
                    apply_drained_identity_if_unchanged(dst, src, identity_baseline);
                }
                if runtime_changed.contains(&src.id) {
                    apply_poller_runtime_if_unchanged(dst, src, identity_baseline);
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(
                target: "session.sync",
                "drain_and_persist task failed: {e}",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_identity_reapply_honors_concurrent_generation_and_marker_writes() {
        let baseline = (
            Some("old-sid".to_string()),
            Some("old-marker".to_string()),
            Some("generation-a".to_string()),
            None,
            None,
            0,
            crate::session::Status::Idle,
        );
        let mut drained = Instance::new("session", "/tmp/project");
        drained.agent_session_id = Some("captured-sid".to_string());
        drained.resume_probe_failed_sid = None;
        drained.omp_capture_generation = Some("generation-a".to_string());

        let mut relaunched = Instance::new("session", "/tmp/project");
        relaunched.agent_session_id = Some("old-sid".to_string());
        relaunched.resume_probe_failed_sid = Some("old-marker".to_string());
        relaunched.omp_capture_generation = Some("generation-b".to_string());
        apply_drained_identity_if_unchanged(&mut relaunched, &drained, &baseline);
        assert_eq!(
            relaunched.omp_capture_generation.as_deref(),
            Some("generation-b")
        );
        assert_eq!(relaunched.agent_session_id.as_deref(), Some("old-sid"));

        let mut marker_changed = Instance::new("session", "/tmp/project");
        marker_changed.agent_session_id = Some("old-sid".to_string());
        marker_changed.resume_probe_failed_sid = Some("peer-marker".to_string());
        marker_changed.omp_capture_generation = Some("generation-a".to_string());
        apply_drained_identity_if_unchanged(&mut marker_changed, &drained, &baseline);
        assert_eq!(
            marker_changed.agent_session_id.as_deref(),
            Some("captured-sid")
        );
        assert_eq!(
            marker_changed.resume_probe_failed_sid.as_deref(),
            Some("peer-marker")
        );
    }

    #[test]
    fn poller_runtime_reapply_keeps_a_deferred_retry() {
        let baseline: SessionIdentityBaseline = (
            None,
            None,
            None,
            None,
            None,
            0,
            crate::session::Status::Idle,
        );
        let mut live = Instance::new("session", "/tmp/project");
        let mut repaired = live.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        repaired.session_id_poller_retry_after = Some(deadline);

        apply_poller_runtime_if_unchanged(&mut live, &repaired, &baseline);

        assert_eq!(live.session_id_poller_retry_after, Some(deadline));

        let concurrent = std::time::Instant::now() + std::time::Duration::from_secs(60);
        live.session_id_poller_retry_after = Some(concurrent);
        apply_poller_runtime_if_unchanged(&mut live, &repaired, &baseline);
        assert_eq!(live.session_id_poller_retry_after, Some(concurrent));

        live.session_id_poller_retry_after = None;
        live.capture_started_at = Some(std::time::SystemTime::now());
        apply_poller_runtime_if_unchanged(&mut live, &repaired, &baseline);
        assert_eq!(
            live.session_id_poller_retry_after, None,
            "a concurrent non-OMP relaunch must reject the stale runtime state"
        );

        live.capture_started_at = None;
        live.status = crate::session::Status::Stopped;
        apply_poller_runtime_if_unchanged(&mut live, &repaired, &baseline);
        assert_eq!(live.session_id_poller_retry_after, None);

        live.status = crate::session::Status::Idle;
        live.lifecycle_generation = 1;
        apply_poller_runtime_if_unchanged(&mut live, &repaired, &baseline);
        assert_eq!(live.session_id_poller_retry_after, None);
    }
}

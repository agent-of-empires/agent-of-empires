//! Draining captured agent session ids onto the rows they belong to.

use crate::session::Instance;
use std::sync::Arc;

use super::state::AppState;

pub(super) type SessionIdentityBaseline = (
    crate::session::ConversationState,
    Option<String>,
    Option<String>,
    Option<std::time::Instant>,
    Option<std::time::SystemTime>,
    u64,
    crate::session::Status,
);

/// Preserve concurrent changes to any part of the native conversation.
pub(super) fn apply_drained_identity_if_unchanged(
    live: &mut Instance,
    drained: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    let (baseline_conversation, baseline_marker, baseline_generation, _, _, _, _) = baseline;
    if baseline_conversation.matches(live) && live.omp_capture_generation == *baseline_generation {
        live.adopt_conversation_state(drained.conversation_state());
        live.omp_capture_generation = drained.omp_capture_generation.clone();
        if live.resume_probe_failed_sid == *baseline_marker {
            live.resume_probe_failed_sid = drained.resume_probe_failed_sid.clone();
        }
    }
}

/// Whether the row's lifecycle still matches the walk's baseline. A relaunch that lands while the
/// walk runs on its clone moves the lifecycle, and the walk's schedule verdict is then dropped:
/// a cleared row is due at once, which costs at most one extra repair walk on the next tick.
fn apply_poller_repair_if_lifecycle_unchanged(
    live: &mut Instance,
    backoff: &crate::session::poller::PollerRepairBackoff,
    baseline: &SessionIdentityBaseline,
) {
    if live.lifecycle_generation == baseline.5 {
        live.poller_repair = backoff.clone();
    }
}

fn apply_poller_runtime_if_unchanged(
    live: &mut Instance,
    repaired: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    if live.active_execution == repaired.active_execution
        && live.omp_capture_generation == repaired.omp_capture_generation
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
    // Drain poller observations into sessions.json so daemon-only sessions persist
    // post-`/clear` sids.
    let snapshot = state.instances.read().await.clone();
    let file_watch = state.file_watch.clone();
    match tokio::task::spawn_blocking(move || {
        let baseline: std::collections::HashMap<String, SessionIdentityBaseline> = snapshot
            .iter()
            .map(|inst| {
                (
                    inst.id.clone(),
                    (
                        inst.conversation_state(),
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
        // Preserve a final queued observation before replacing a stopped worker.
        let outcome =
            crate::session::sync::drain_and_persist_session_ids(&mut snapshot, &file_watch);
        // One observation for the whole repair walk, as on the TUI side.
        let live = crate::tmux::LiveSessionSnapshot::new();
        let backoff_before = repair_backoffs(&snapshot);
        let runtime_changed: std::collections::HashSet<String> = snapshot
            .iter_mut()
            .filter_map(|inst| {
                let retry_before = inst.session_id_poller_retry_after;
                let started = inst.repair_session_id_poller_if_needed(&live);
                (started || inst.session_id_poller_retry_after != retry_before)
                    .then(|| inst.id.clone())
            })
            .collect();
        // The walk ran on a clone.
        let deferred = changed_repair_backoffs(&backoff_before, &snapshot);
        (outcome, snapshot, baseline, runtime_changed, deferred)
    })
    .await
    {
        Ok((outcome, mutated, baseline, runtime_changed, deferred))
            if outcome.touched() || !runtime_changed.is_empty() || !deferred.is_empty() =>
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
                if let Some(backoff) = deferred.get(&src.id) {
                    apply_poller_repair_if_lifecycle_unchanged(dst, backoff, identity_baseline);
                }
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

/// Snapshot each row's poller-repair schedule before the repair walk.
fn repair_backoffs(
    instances: &[crate::session::Instance],
) -> std::collections::HashMap<String, crate::session::poller::PollerRepairBackoff> {
    instances
        .iter()
        .map(|inst| (inst.id.clone(), inst.poller_repair.clone()))
        .collect()
}

/// Rows whose poller-repair schedule the walk changed (a deferral, a re-probe, or a reset),
/// keyed by id.
fn changed_repair_backoffs(
    before: &std::collections::HashMap<String, crate::session::poller::PollerRepairBackoff>,
    after: &[crate::session::Instance],
) -> std::collections::HashMap<String, crate::session::poller::PollerRepairBackoff> {
    after
        .iter()
        .filter(|inst| before.get(&inst.id) != Some(&inst.poller_repair))
        .map(|inst| (inst.id.clone(), inst.poller_repair.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_identity_reapply_honors_concurrent_generation_and_marker_writes() {
        let baseline = (
            crate::session::ConversationState {
                session_id: Some("old-sid".into()),
                ..Instance::new("session", "/tmp/project").conversation_state()
            },
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
        let mut peer = Instance::new("session", "/tmp/project");
        peer.adopt_conversation_state(baseline.0.clone());
        peer.omp_capture_generation = baseline.2.clone();
        peer.pi_session_path = Some("/peer/transcript.jsonl".into());
        let expected = peer.conversation_state();
        apply_drained_identity_if_unchanged(&mut peer, &drained, &baseline);
        assert_eq!(peer.conversation_state(), expected);
    }

    #[test]
    fn a_relaunch_during_the_walk_keeps_its_schedule_clear() {
        let mut live = Instance::new("session", "/tmp/project");
        live.lifecycle_generation = 7;
        let baseline: SessionIdentityBaseline = (
            live.conversation_state(),
            None,
            None,
            None,
            None,
            7,
            crate::session::Status::Idle,
        );
        let mut walked = live.clone();
        let now = std::time::Instant::now();
        walked.poller_repair.reprobe(now);

        // Same lifecycle: the walk's schedule lands.
        live.poller_repair = Default::default();
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            Some(std::time::Duration::from_secs(5))
        );

        // A relaunch landed: its clear stands, and the walk's schedule is dropped.
        live.lifecycle_generation = 8;
        let mut relaunched = live.clone();
        // A relaunch that reached the launch stamp: new start time, cleared schedule.
        relaunched.last_start_time = Some(std::time::Instant::now());
        relaunched.poller_repair.reset();
        live.merge_post_restart_with_baseline(&live.clone(), &relaunched);
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            None,
            "the relaunch replaced the poller this schedule paced"
        );
    }

    #[test]
    fn poller_runtime_reapply_keeps_a_deferred_retry() {
        let baseline: SessionIdentityBaseline = (
            Instance::new("session", "/tmp/project").conversation_state(),
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

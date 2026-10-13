//! Draining captured agent session ids onto the rows they belong to.

use crate::session::Instance;
use std::sync::Arc;

use super::state::AppState;

/// What the walk inspected before it ran, so a verdict can be dropped if the row moved. The last
/// field is the agent: a tool swap changes it without moving the lifecycle counter, so the guard
/// has to read it separately.
pub(super) type SessionIdentityBaseline = (
    crate::session::ConversationState,
    Option<String>,
    Option<String>,
    Option<std::time::Instant>,
    Option<std::time::SystemTime>,
    u64,
    crate::session::Status,
    String,
);

/// Preserve concurrent changes to any part of the native conversation.
pub(super) fn apply_drained_identity_if_unchanged(
    live: &mut Instance,
    drained: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    let (baseline_conversation, baseline_marker, baseline_generation, _, _, _, _, _) = baseline;
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
    if live.lifecycle_generation == baseline.5
        && live.active_execution == baseline.0.active
        && live.omp_capture_generation == baseline.2
        && live.tool == baseline.7
    {
        live.poller_repair = backoff.clone();
    }
}

fn apply_poller_runtime_if_unchanged(
    live: &mut Instance,
    repaired: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    if live.tool == repaired.tool
        && live.active_execution == repaired.active_execution
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

fn apply_drained_lifecycle_if_unchanged(
    live: &mut Instance,
    drained: &Instance,
    baseline: &SessionIdentityBaseline,
) {
    let baseline_generation = baseline.5;
    if live.lifecycle_generation == baseline_generation {
        live.lifecycle_generation = drained.lifecycle_generation;
        live.lifecycle_reservation = drained.lifecycle_reservation.clone();
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
                        inst.tool.clone(),
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
            let lifecycle_advanced: std::collections::HashSet<&str> = outcome
                .lifecycle_advanced
                .iter()
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
                if lifecycle_advanced.contains(src.id.as_str()) {
                    apply_drained_lifecycle_if_unchanged(dst, src, identity_baseline);
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
mod migrated_tui_identity_tests {
    use crate::server::{test_support, AppState};
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::{GroupTree, Instance, Storage};
    use serial_test::serial;
    use tempfile::TempDir;
    fn setup_test_home(temp: &TempDir) -> AppDirGuard {
        isolate_app_dir_at(temp.path())
    }
    use crate::session::poller::SessionPoller;
    use crate::session::{ResumeIntent, View};
    use std::sync::{Arc, Mutex};

    const NEW_SID: &str = "019342ab-1111-7aaa-8bbb-cccdddeeefff";

    struct TmuxSession(String);

    impl TmuxSession {
        fn create(id: &str, title: &str) -> Self {
            Self::create_named(crate::tmux::Session::generate_name(id, title))
        }

        fn create_terminal(id: &str, title: &str) -> Self {
            Self::create_named(crate::tmux::TerminalSession::generate_name(id, title))
        }

        fn create_named(name: String) -> Self {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &name])
                .output();
            let status = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &name])
                .status()
                .expect("failed to spawn tmux");
            assert!(status.success(), "tmux new-session failed for {}", name);
            crate::tmux::refresh_session_cache();
            Self(name)
        }
        fn name(&self) -> &str {
            &self.0
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
            crate::tmux::refresh_session_cache();
        }
    }

    fn skip_if_no_tmux() -> bool {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("Skipping: tmux not available");
            return true;
        }
        false
    }

    fn captured_env(name: &str) -> Option<String> {
        crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
    }

    async fn build_state_with_inst(profile: &str, inst: &Instance) -> Arc<AppState> {
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, groups| {
                *rows = vec![inst.clone()];
                *groups =
                    GroupTree::new_with_groups(std::slice::from_ref(inst), &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
        let state = test_support::build_test_app_state(vec![inst.clone()]);
        test_support::refresh_canonical_metadata_for_test(&state).await;
        state
    }
    async fn attach_poller_with_update(state: &Arc<AppState>, id: &str, sid: &str) {
        let mut rows = state.instances.write().await;
        test_support::attach_session_id_update_for_test(
            rows.iter_mut().find(|row| row.id == id).unwrap(),
            sid,
        );
    }
    async fn memory_sid(state: &Arc<AppState>, id: &str) -> Option<String> {
        state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .agent_session_id
            .clone()
    }
    async fn stop_pollers(state: &Arc<AppState>) {
        for row in state.instances.read().await.iter() {
            if let Some(poller) = &row.session_id_poller {
                poller.lock().unwrap_or_else(|p| p.into_inner()).stop();
            }
        }
    }
    fn fresh_instance(profile: &str, title: &str) -> Instance {
        let mut inst = Instance::new(title, "/tmp/x");
        inst.tool = "claude".to_string();
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;
        inst
    }

    /// An applied CAS publishes the new sid to whichever pane the row runs in: the agent
    /// session by default, the paired terminal for terminal rows, and nowhere without a pane.
    #[tokio::test]
    #[serial]
    async fn daemon_identity_publishes_after_cas() {
        if skip_if_no_tmux() {
            return;
        }
        // (profile, terminal row, create a pane)
        for (profile, terminal, with_pane) in [
            ("apply-publish", false, true),
            ("apply-terminal-publish", true, true),
            ("apply-pane-dead", false, false),
        ] {
            let temp = TempDir::new().unwrap();
            let _guard = setup_test_home(&temp);
            let mut inst = fresh_instance(profile, profile);
            if terminal {
                inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
            }
            let state = build_state_with_inst(profile, &inst).await;
            let agent_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let tmux = with_pane.then(|| {
                if terminal {
                    TmuxSession::create_terminal(&inst.id, &inst.title)
                } else {
                    TmuxSession::create(&inst.id, &inst.title)
                }
            });

            attach_poller_with_update(&state, &inst.id, NEW_SID).await;

            test_support::drain_session_id_updates_for_test(&state).await;
            let mem_sid = memory_sid(&state, &inst.id).await;
            assert_eq!(
                Storage::new_unwatched(profile).unwrap().load().unwrap()[0]
                    .agent_session_id
                    .as_deref(),
                Some(NEW_SID)
            );
            assert_eq!(mem_sid.as_deref(), Some(NEW_SID), "{profile}");
            match &tmux {
                Some(tmux) => {
                    assert_eq!(
                        captured_env(tmux.name()).as_deref(),
                        Some(NEW_SID),
                        "{profile}"
                    );
                    if terminal {
                        assert!(captured_env(&agent_name).is_none(), "{profile}");
                    }
                }
                None => assert!(
                    captured_env(&agent_name).is_none(),
                    "{profile}: no tmux session means no publish target"
                ),
            }
            stop_pollers(&state).await;
        }
    }

    /// A sid filtered before the CAS (retroactively excluded or invalid) stays out of memory,
    /// and the pane env converges on the disk-backed mirror (None) rather than the stale value.
    #[tokio::test]
    #[serial]
    async fn daemon_identity_filtered_sid_clears_env() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        // (profile, sid the poller reports, exclude it retroactively)
        for (profile, sid, exclude) in [
            ("apply-excludes", NEW_SID, true),
            ("apply-invalid", "bad sid!", false),
        ] {
            let inst = fresh_instance(profile, profile);
            let state = build_state_with_inst(profile, &inst).await;
            if exclude {
                if let Some(i) = state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|row| row.id == inst.id)
                {
                    i.retroactive_capture_excludes.insert(
                        crate::session::ConversationBinding::unknown(sid.to_string()),
                    );
                }
            }

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-untouched",
            )
            .unwrap();

            attach_poller_with_update(&state, &inst.id, sid).await;

            let before =
                std::fs::read(Storage::new_unwatched(profile).unwrap().sessions_path()).unwrap();
            test_support::drain_session_id_updates_for_test(&state).await;
            assert_eq!(
                std::fs::read(Storage::new_unwatched(profile).unwrap().sessions_path()).unwrap(),
                before
            );
            let mem_sid = memory_sid(&state, &inst.id).await;
            assert!(mem_sid.is_none(), "{profile}: filtered sid entered memory");
            assert!(
                captured_env(tmux.name()).is_none(),
                "{profile}: env must converge on disk (None)"
            );
            stop_pollers(&state).await;
        }
    }

    #[tokio::test]
    #[serial]
    async fn daemon_identity_skipped_publishes_disk_value() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-skipped";
        let peer_sid = "019342aa-3333-7eee-8fff-aaaabbbbcccc";
        let other_peer = "019342bb-4444-7fff-8000-111122223333";

        let mut inst = fresh_instance(profile, "ase");
        inst.agent_session_id = Some(peer_sid.to_string());
        let state = build_state_with_inst(profile, &inst).await;

        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|i, _g| {
                i[0].agent_session_id = Some(other_peer.to_string());
                Ok(())
            })
            .unwrap();

        let tmux = TmuxSession::create(&inst.id, &inst.title);
        crate::tmux::env::set_hidden_env(
            tmux.name(),
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            NEW_SID,
        )
        .unwrap();

        attach_poller_with_update(&state, &inst.id, NEW_SID).await;

        test_support::drain_session_id_updates_for_test(&state).await;
        let mem_sid = memory_sid(&state, &inst.id).await;
        assert_eq!(
            Storage::new_unwatched(profile).unwrap().load().unwrap()[0]
                .agent_session_id
                .as_deref(),
            Some(other_peer)
        );
        assert_eq!(
            mem_sid.as_deref(),
            Some(other_peer),
            "memory rolls back to disk after CAS skip"
        );
        assert_eq!(
            captured_env(tmux.name()).as_deref(),
            Some(other_peer),
            "env converges from poller's pre-published NEW_SID to disk's other_peer"
        );
        stop_pollers(&state).await;
    }

    #[tokio::test]
    #[serial]
    async fn daemon_identity_repair_skips_structured_and_repairs_live_terminal() {
        if skip_if_no_tmux() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _guard = setup_test_home(&temp);

        let profile = "apply-poller-repair";
        let terminal = fresh_instance(profile, "repair-terminal");
        let mut structured = fresh_instance(profile, "repair-structured");
        structured.view = View::Structured;
        let state = build_state_with_inst(profile, &terminal).await;
        Storage::new_unwatched(profile)
            .unwrap()
            .update(|rows, _| {
                rows.push(structured.clone());
                Ok(())
            })
            .unwrap();
        state.instances.write().await.push(structured.clone());
        let terminal_stopped = Arc::new(Mutex::new(SessionPoller::new(
            "stopped".into(),
            "claude".into(),
            None,
        )));
        let structured_stopped = Arc::new(Mutex::new(SessionPoller::new(
            "stopped".into(),
            "claude".into(),
            None,
        )));
        {
            let mut rows = state.instances.write().await;
            rows.iter_mut()
                .find(|row| row.id == terminal.id)
                .unwrap()
                .session_id_poller = Some(terminal_stopped.clone());
            rows.iter_mut()
                .find(|row| row.id == structured.id)
                .unwrap()
                .session_id_poller = Some(structured_stopped.clone());
        }
        let _tmux = TmuxSession::create(&terminal.id, &terminal.title);
        let before =
            std::fs::read(Storage::new_unwatched(profile).unwrap().sessions_path()).unwrap();
        test_support::drain_session_id_updates_for_test(&state).await;
        let rows = state.instances.read().await;
        let repaired = rows
            .iter()
            .find(|row| row.id == terminal.id)
            .unwrap()
            .session_id_poller
            .clone()
            .expect("live pane replacement poller");
        assert!(!Arc::ptr_eq(&repaired, &terminal_stopped));
        assert!(repaired
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_running());
        assert!(Arc::ptr_eq(
            rows.iter()
                .find(|row| row.id == structured.id)
                .unwrap()
                .session_id_poller
                .as_ref()
                .unwrap(),
            &structured_stopped
        ));
        drop(rows);
        assert_eq!(
            std::fs::read(Storage::new_unwatched(profile).unwrap().sessions_path()).unwrap(),
            before
        );
        repaired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stop();
    }
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
            "claude".to_string(),
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
            "claude".to_string(),
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

        // A relaunch landed: its clear stands, and the walk's verdict is dropped. The counter is
        // moved here rather than by the merge below, which is the point: the guard reads it alone.
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
            "a lifecycle that moved under the walk discards its verdict"
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
            "claude".to_string(),
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

    #[test]
    fn drained_lifecycle_reapply_keeps_a_concurrent_reservation() {
        let baseline: SessionIdentityBaseline = (
            Instance::new("session", "/tmp/project").conversation_state(),
            None,
            None,
            None,
            None,
            0,
            crate::session::Status::Idle,
            "claude".to_string(),
        );
        let mut drained = Instance::new("session", "/tmp/project");
        drained.lifecycle_generation = 7;
        drained.lifecycle_reservation = Some(crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Capture,
            generation: 7,
            at: chrono::Utc::now(),
        });

        // Nothing moved under the guard, so the drained values land.
        let mut quiet = Instance::new("session", "/tmp/project");
        apply_drained_lifecycle_if_unchanged(&mut quiet, &drained, &baseline);
        assert_eq!(quiet.lifecycle_generation, 7);
        assert_eq!(quiet.lifecycle_reservation, drained.lifecycle_reservation);

        // A relaunch advanced the generation while the drain ran: copying the
        // drained values would roll that reservation back, so it must not happen.
        let mut relaunched = Instance::new("session", "/tmp/project");
        let reservation = crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Launch,
            generation: 9,
            at: chrono::Utc::now(),
        };
        relaunched.lifecycle_generation = 9;
        relaunched.lifecycle_reservation = Some(reservation.clone());
        apply_drained_lifecycle_if_unchanged(&mut relaunched, &drained, &baseline);
        assert_eq!(relaunched.lifecycle_generation, 9);
        assert_eq!(relaunched.lifecycle_reservation, Some(reservation));
    }

    /// A tool swap or a disk write can move a row's execution without moving its lifecycle counter,
    /// because neither claims one. A walk that armed its window for the old execution must not
    /// write it onto the new one.
    #[test]
    fn a_walk_verdict_is_dropped_when_the_execution_moved_under_it() {
        let execution = |id: &str| {
            Some(crate::session::ActiveExecution {
                launch_id: id.to_string(),
                binding: crate::session::ExecutionBinding {
                    agent: "claude".into(),
                    stores: Vec::new(),
                    configuration: Vec::new(),
                    cwd: "/tmp".into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: None,
                },
                capture: None,
                container: None,
            })
        };
        let mut live = Instance::new("swapped", "/tmp/swapped");
        live.lifecycle_generation = 7;
        let baseline: SessionIdentityBaseline = (
            live.conversation_state(),
            None,
            None,
            None,
            None,
            7,
            crate::session::Status::Idle,
            "claude".to_string(),
        );
        let mut walked = live.clone();
        walked.poller_repair.reprobe(std::time::Instant::now());

        live.poller_repair = Default::default();
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            Some(std::time::Duration::from_secs(5)),
            "fixture: nothing moved, so the verdict lands"
        );

        // The row swapped tool under the walk: same lifecycle counter, different execution.
        live.active_execution = execution("launch-2");
        live.poller_repair = Default::default();
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            None,
            "a window armed for the execution the row gave up is not written onto the new one"
        );
    }

    /// A tool swap moves the agent without the lifecycle counter, the execution or the capture
    /// generation, so a walk that armed a window for the previous agent can still pass every
    /// other check. The baseline's agent is what tells the two apart.
    #[test]
    fn a_walk_verdict_is_dropped_when_the_agent_moved_under_it() {
        let mut live = Instance::new("swapped-agent", "/tmp/swapped-agent");
        live.lifecycle_generation = 7;
        let baseline: SessionIdentityBaseline = (
            live.conversation_state(),
            None,
            None,
            None,
            None,
            7,
            crate::session::Status::Idle,
            "claude".to_string(),
        );
        let mut walked = live.clone();
        walked.poller_repair.reprobe(std::time::Instant::now());

        live.poller_repair = Default::default();
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            Some(std::time::Duration::from_secs(5)),
            "fixture: nothing moved, so the verdict lands"
        );

        live.tool = "codex".to_string();
        live.poller_repair = Default::default();
        apply_poller_repair_if_lifecycle_unchanged(&mut live, &walked.poller_repair, &baseline);
        assert_eq!(
            live.poller_repair.current_reprobe_delay(),
            None,
            "a window armed for the previous agent is not written onto this one"
        );
    }
}

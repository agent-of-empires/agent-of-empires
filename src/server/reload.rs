//! Reloading session rows from disk and merging them onto what the daemon
//! already holds in memory.

use crate::file_watch::FileWatchService;
use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;
use std::sync::Arc;

use super::state::{AppState, StatusSource};
use super::structured_repair::{
    persist_structured_row_repairs, repair_structured_rows_from_live_workers,
    LiveStructuredWorkerRecord,
};

/// Load sessions from all profiles, matching the TUI's "all profiles" view.
pub(super) fn load_all_instances(
    file_watch: &Arc<FileWatchService>,
) -> anyhow::Result<Vec<Instance>> {
    let profiles = match crate::session::list_profiles() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                error = %e,
                "list_profiles failed; load_all_instances returning empty set"
            );
            return Ok(Vec::new());
        }
    };
    let mut all = Vec::new();
    for profile in &profiles {
        match Storage::new(profile, file_watch.clone()).and_then(|s| s.load()) {
            Ok(mut instances) => {
                for inst in &mut instances {
                    inst.source_profile = profile.clone();
                }
                all.extend(instances);
            }
            Err(e) => {
                tracing::warn!(
                    target: "server.file_watch",
                    profile = %profile,
                    error = %e,
                    "load_all_instances skipped profile; sessions for this profile will be \
                     absent from state until next successful reload"
                );
            }
        }
    }
    Ok(all)
}

/// Carry over the in-memory-only fields from the prior `state.instances` entry into the
/// freshly-loaded one.
pub(super) fn merge_runtime_fields(prior: Instance, mut fresh: Instance) -> Instance {
    if fresh.active_execution == prior.active_execution {
        fresh.session_id_poller = prior.session_id_poller;
        fresh.poller_repair = prior.poller_repair;
        fresh.session_id_poller_retry_after = prior.session_id_poller_retry_after;
    } else {
        prior.stop_poller();
    }
    fresh.last_error_check = prior.last_error_check;
    fresh.last_start_time = prior.last_start_time;
    if fresh.status == Status::Error {
        fresh.last_error = prior.last_error;
    }
    fresh.acp_load_session_capable = prior.acp_load_session_capable;
    fresh
}

/// The prior tick's `#[serde(skip)]` status bookkeeping for one row, the
/// input to [`seed_tick_tracking`].
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct PriorTickTracking {
    ever_confirmed_present: bool,
    unknown_since: Option<std::time::Instant>,
    detection: crate::session::DetectionState,
}

impl PriorTickTracking {
    pub(super) fn of(inst: &Instance) -> Self {
        Self {
            ever_confirmed_present: inst.ever_confirmed_present,
            unknown_since: inst.unknown_since,
            detection: inst.detection,
        }
    }
}

/// Carry the previous tick's status bookkeeping onto a freshly disk-loaded instance, keyed
/// by id.
pub(super) fn seed_tick_tracking(
    instances: &mut [Instance],
    prev: &std::collections::HashMap<String, PriorTickTracking>,
) {
    for inst in instances {
        if let Some(prior) = prev.get(&inst.id) {
            inst.ever_confirmed_present = prior.ever_confirmed_present;
            inst.unknown_since = prior.unknown_since;
            inst.detection = prior.detection;
        }
    }
}

/// One tick's per-instance status decision.
pub(super) fn apply_tick_status_decisions(
    instances: &mut [Instance],
    prev: &std::collections::HashMap<String, crate::session::Status>,
    suppressed_ids: &std::collections::HashSet<String>,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
) {
    for inst in instances.iter_mut() {
        if suppressed_ids.contains(&inst.id) {
            inst.status = Status::Starting;
            continue;
        }
        inst.live_status_baseline = prev.get(&inst.id).copied();
        // A trashed row remains in storage until its retention period ends, but it is no
        // longer a live session.
        if inst.is_trashed() {
            if let Some(live) = inst.live_status_baseline {
                inst.status = live;
            }
            continue;
        }
        if skip_tmux_decision_for_structured(inst) {
            continue;
        }
        let Some(pane_metadata) = pane_metadata else {
            // A failed batch probe says nothing about any individual pane.
            if let Some(live) = inst.live_status_baseline {
                inst.status = live;
            }
            continue;
        };
        let session_name = crate::tmux::resolve_agent_session_name_in(
            pane_metadata,
            &inst.id,
            &crate::tmux::Session::generate_name(&inst.id, &inst.title),
        );
        inst.update_status_with_metadata(pane_metadata.get(&session_name), Some(&session_name));
    }
}

/// The real status transitions this tick observed, as `(index into instances, previous
/// status)` pairs.
pub(super) fn observed_transitions(
    instances: &[Instance],
    prev: &std::collections::HashMap<String, crate::session::Status>,
) -> Vec<(usize, Status)> {
    instances
        .iter()
        .enumerate()
        .filter_map(|(idx, inst)| {
            let old = *prev.get(&inst.id)?;
            (old != inst.status).then_some((idx, old))
        })
        .collect()
}

/// Report whether the caller must skip the tmux status decision for this row, carrying the
/// acp-authoritative live status onto it when so.
pub(super) fn skip_tmux_decision_for_structured(inst: &mut Instance) -> bool {
    if !inst.is_structured() {
        return false;
    }
    inst.clear_stale_tmux_error();
    // `None` means the row is newer than the last tick and has no live value
    // yet; its disk status is all there is, and the absent baseline already
    // suppresses a transition report.
    if let Some(live) = inst.live_status_baseline {
        inst.status = live;
    }
    true
}

// INVARIANTS for `reload_state_instances_from_disk` (do not break without revisiting
// `tests/serve_disk_reload_helper_equivalence.rs`).

/// Reload `state.instances` by merging caller-supplied `fresh` against the prior in-memory
/// snapshot per id, then reapplying the acp overlay.
pub(super) struct PriorById(std::collections::HashMap<String, Instance>);

impl PriorById {
    fn drain_from(current: &mut Vec<Instance>) -> Self {
        Self(
            current
                .drain(..)
                .map(|inst| (inst.id.clone(), inst))
                .collect(),
        )
    }

    fn get(&self, id: &str) -> Option<&Instance> {
        self.0.get(id)
    }
}

#[doc(hidden)]
pub(crate) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<LiveStructuredWorkerRecord>,
    status_source: StatusSource,
    read_epoch: u64,
) {
    // Snapshot suppression here so a worker that unmarks between the caller's input build
    // and the per-id decision cannot combine a cleared mark with a stale row to re-emit the
    // phantom Error transition the suppression exists to prevent.
    let suppressed_ids =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
    // Repair is a view transition too.
    let terminal_ids: std::collections::HashSet<&str> = if live_worker_records.is_empty() {
        std::collections::HashSet::new()
    } else {
        fresh
            .iter()
            .filter(|row| !row.is_structured())
            .map(|row| row.id.as_str())
            .collect()
    };
    let mut repair_records = Vec::new();
    let mut repair_guards = Vec::new();
    for (record, _) in live_worker_records {
        if !terminal_ids.contains(record.session_id.as_str()) {
            continue;
        }
        let lock = state.instance_lock(&record.session_id).await;
        let Ok(guard) = lock.try_lock_owned() else {
            continue;
        };
        // The caller may have sampled this runner before disable finished.
        let Ok(Some(current)) = crate::process::worker_registry::load(&record.session_id) else {
            continue;
        };
        if current.pid != record.pid
            || current.started_at != record.started_at
            || !crate::process::worker_registry::is_record_live(&current)
        {
            continue;
        }
        let Some(acp_session_id) = current
            .stored_acp_session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        repair_records.push((current, acp_session_id));
        repair_guards.push(guard);
    }

    let mut current = state.instances.write().await;

    // Invariant 8.
    let current_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    if current_epoch != read_epoch {
        tracing::debug!(
            target: "server.file_watch",
            read_epoch,
            current_epoch,
            "dropping a disk reload whose snapshot predates a session lifecycle mutation"
        );
        return;
    }

    let prior_by_id = PriorById::drain_from(&mut current);

    let mut merged: Vec<Instance> = Vec::with_capacity(fresh.len());
    for mut row in fresh {
        if let Some(prior) = prior_by_id.get(&row.id).cloned() {
            let prior_status = prior.status;
            let prior_last_accessed = prior.last_accessed_at;
            let prior_idle_entered = prior.idle_entered_at;
            let prior_tracking = PriorTickTracking::of(&prior);
            row = merge_runtime_fields(prior, row);
            match status_source {
                StatusSource::DiskOnly => {
                    row.status = prior_status;
                    row.idle_entered_at = prior_idle_entered.or(row.idle_entered_at);
                    // `row` here is a raw disk load (no tmux scrape ran), so
                    // the `#[serde(skip)]` tracking fields are still at their
                    // zeroed defaults; restore the prior tick's.
                    row.ever_confirmed_present = prior_tracking.ever_confirmed_present;
                    row.unknown_since = prior_tracking.unknown_since;
                    row.detection = prior_tracking.detection;
                }
                StatusSource::TmuxApplied => {
                    // Caller already applied tmux scrape to fresh.status; that is the
                    // authoritative value.
                }
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }

    let repairs = repair_structured_rows_from_live_workers(&mut merged, repair_records);

    apply_acp_overlay_inplace(&prior_by_id, &mut merged);

    *current = merged;
    drop(current);

    persist_structured_row_repairs(state, repairs, repair_guards);
}

/// Apply the acp status / timestamps overlay to `merged`, sourcing values from
/// `prior_by_id`.
pub(super) fn apply_acp_overlay_inplace(prior_by_id: &PriorById, merged: &mut [Instance]) {
    for inst in merged.iter_mut() {
        if !inst.is_structured() {
            continue;
        }
        let Some(prior) = prior_by_id.get(&inst.id) else {
            continue;
        };
        inst.status = prior.status;
        inst.last_accessed_at = prior.last_accessed_at;
        inst.idle_entered_at = prior.idle_entered_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn live_repair_fixture() -> (Arc<AppState>, Instance, LiveStructuredWorkerRecord, Storage) {
        let mut row = Instance::new("repair-transition", "/tmp/repo");
        row.source_profile = "repair-transition".into();
        let storage = Storage::new_unwatched(&row.source_profile).unwrap();
        storage
            .update(|instances, _| {
                instances.push(row.clone());
                Ok(())
            })
            .unwrap();
        let socket = crate::process::worker_registry::socket_path_for(&row.id).unwrap();
        crate::process::worker_registry::touch_live_socket(&socket);
        let record = crate::process::worker_registry::WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            socket,
            "codex-acp".into(),
            "codex".into(),
            "/tmp/repo".into(),
            None,
            vec![],
            vec![],
            Some("agent-session".into()),
            Some(row.source_profile.clone()),
        );
        crate::process::worker_registry::save(&record).unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        (state, row, (record, "agent-session".into()), storage)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_does_not_undo_terminal_transition_during_teardown() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        // Disable committed terminal view, but session/delete is still running.
        let lock = state.instance_lock(&row.id).await;
        let _transition = lock.lock().await;
        state
            .mutation_epoch
            .store(1, std::sync::atomic::Ordering::SeqCst);
        reload_state_instances_from_disk(
            &state,
            vec![row],
            vec![record],
            StatusSource::DiskOnly,
            1,
        )
        .await;
        assert!(!state.instances.read().await[0].is_structured());
        assert!(!storage.load().unwrap()[0].is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_rejects_a_registry_sample_retired_after_the_disk_read() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        // The reload sampled the runner during teardown; disable finished
        // before this reload could acquire the transition lock.
        crate::process::worker_registry::delete_if_owned(&row.id, std::process::id()).unwrap();
        reload_state_instances_from_disk(
            &state,
            vec![row],
            vec![record],
            StatusSource::DiskOnly,
            0,
        )
        .await;
        assert!(!state.instances.read().await[0].is_structured());
        assert!(!storage.load().unwrap()[0].is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_commits_before_a_following_terminal_transition() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        let lock = state.instance_lock(&row.id).await;
        reload_state_instances_from_disk(
            &state,
            vec![row.clone()],
            vec![record],
            StatusSource::DiskOnly,
            0,
        )
        .await;
        assert!(state.instances.read().await[0].is_structured());
        // The following view transition must observe the durable repair,
        // rather than race an older queued structured write.
        let _transition = tokio::time::timeout(std::time::Duration::from_secs(2), lock.lock())
            .await
            .expect("repair persistence must finish");
        assert!(storage.load().unwrap()[0].is_structured());
    }

    /// A structured row as the poll loop finds it mid-phantom.
    fn phantom_structured_row(id: &str) -> Instance {
        let mut inst = Instance::new(id, "/tmp/test");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;
        inst
    }

    /// The other half of #2690 / #2697: for a structured row the live ACP status is
    /// authoritative, so the tmux decision is skipped, the baseline stays in step with
    /// the carried status, and a tmux error left over from a converted terminal session
    /// is cleared. A tmux-backed row keeps every field for the poller instead.
    #[test]
    fn skip_tmux_decision_for_structured_carries_the_live_status() {
        let mut carried = phantom_structured_row("acp-session");
        carried.live_status_baseline = Some(Status::Error);
        assert!(skip_tmux_decision_for_structured(&mut carried));
        assert_eq!(carried.status, Status::Error);
        assert_eq!(carried.live_status_baseline, Some(carried.status));

        // A row created since the last tick has no live value yet.
        let mut fresh = phantom_structured_row("acp-session");
        fresh.status = Status::Running;
        assert!(skip_tmux_decision_for_structured(&mut fresh));
        assert_eq!(fresh.status, Status::Running);
        assert_eq!(fresh.live_status_baseline, None);

        let mut converted = phantom_structured_row("acp-session");
        converted.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());
        assert!(skip_tmux_decision_for_structured(&mut converted));
        assert_eq!(converted.last_error, None);

        let mut tmux = Instance::new("tmux-session", "/tmp/test");
        tmux.status = Status::Idle;
        tmux.live_status_baseline = Some(Status::Error);
        tmux.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());
        assert!(!skip_tmux_decision_for_structured(&mut tmux));
        assert_eq!(tmux.status, Status::Idle, "disk status is untouched");
        assert_eq!(
            tmux.last_error.as_deref(),
            Some(crate::session::TMUX_SESSION_GONE_ERROR),
            "the poller still needs the tmux error"
        );
    }

    #[test]
    fn tick_reports_no_transition_for_a_structured_phantom() {
        // The regression at tick level, over the two halves together.
        let inst = phantom_structured_row("acp-session");
        let prev = std::collections::HashMap::from([(inst.id.clone(), Status::Error)]);
        let mut instances = vec![inst];

        apply_tick_status_decisions(
            &mut instances,
            &prev,
            &std::collections::HashSet::new(),
            Some(&std::collections::HashMap::new()),
        );

        assert_eq!(
            observed_transitions(&instances, &prev),
            vec![],
            "a structured row whose live status did not move must report no \
             transition, so status_tx stays silent and nothing is persisted or \
             marked unread"
        );
        // Note this holds for *every* structured row, not just a phantom.
    }

    #[test]
    fn tick_skips_a_row_that_is_new_since_the_last_snapshot() {
        // No `prev` entry means the row was created since the last tick; there
        // is no previous status to have transitioned from.
        let mut instances = vec![phantom_structured_row("acp-session")];
        let prev = std::collections::HashMap::new();

        apply_tick_status_decisions(
            &mut instances,
            &prev,
            &std::collections::HashSet::new(),
            Some(&std::collections::HashMap::new()),
        );

        assert_eq!(instances[0].status, Status::Idle, "disk status stands");
        assert_eq!(instances[0].live_status_baseline, None);
        assert_eq!(observed_transitions(&instances, &prev), vec![]);
    }

    #[test]
    fn tick_forces_a_recently_restarted_row_to_starting() {
        // Two things at once.
        let inst = phantom_structured_row("acp-session");
        let id = inst.id.clone();
        let prev = std::collections::HashMap::from([(id.clone(), Status::Error)]);
        let mut instances = vec![inst];

        apply_tick_status_decisions(
            &mut instances,
            &prev,
            &std::collections::HashSet::from([id]),
            Some(&std::collections::HashMap::new()),
        );

        assert_eq!(instances[0].status, Status::Starting);
        assert_eq!(
            observed_transitions(&instances, &prev),
            vec![(0, Status::Error)],
            "a transition the tick does own must still be reported"
        );
    }

    #[test]
    fn tick_holds_tmux_statuses_when_the_batch_probe_fails() {
        for (disk, live) in [
            (Status::Idle, Status::Running),
            (Status::Unknown, Status::Error),
        ] {
            let mut inst = Instance::new("tmux-session", "/tmp/test");
            inst.status = disk;
            let id = inst.id.clone();
            let prev = std::collections::HashMap::from([(id, live)]);
            let mut instances = vec![inst];

            apply_tick_status_decisions(
                &mut instances,
                &prev,
                &std::collections::HashSet::new(),
                None,
            );

            assert_eq!(instances[0].status, live, "disk status was {disk:?}");
            assert_eq!(observed_transitions(&instances, &prev), vec![]);
        }
    }

    #[test]
    fn seed_tick_tracking_carries_prior_tick_fields_onto_fresh_instance() {
        // `load_all_instances` always resets these `#[serde(skip)]` fields to
        // their defaults, mimicking status_poll_loop's fresh disk load.
        let mut fresh = vec![Instance::new("sess-1", "/tmp/seed")];
        assert!(!fresh[0].ever_confirmed_present);
        assert_eq!(fresh[0].unknown_since, None);
        assert_eq!(
            fresh[0].detection,
            crate::session::DetectionState::default()
        );

        let confirmed_at = std::time::Instant::now() - std::time::Duration::from_secs(3);
        let mut prev = std::collections::HashMap::new();
        prev.insert(
            fresh[0].id.clone(),
            PriorTickTracking {
                ever_confirmed_present: true,
                unknown_since: Some(confirmed_at),
                detection: crate::session::DetectionState {
                    pending: Some(Status::Idle),
                    ..Default::default()
                },
            },
        );

        seed_tick_tracking(&mut fresh, &prev);

        assert!(
            fresh[0].ever_confirmed_present,
            "prior tick's ever_confirmed_present must seed the fresh instance \
             before update_status_with_metadata runs on it"
        );
        assert_eq!(
            fresh[0].unknown_since,
            Some(confirmed_at),
            "prior tick's unknown_since must seed the fresh instance so the \
             Unknown->Error escalation window can actually accumulate elapsed \
             time across ticks (#2865)"
        );
        assert_eq!(
            fresh[0].detection.pending,
            Some(Status::Idle),
            "prior tick's proposal must seed the fresh instance so the poll \
             that agrees with it can publish it (#3642)"
        );
    }

    #[test]
    fn seed_tick_tracking_leaves_unknown_ids_untouched() {
        let mut fresh = vec![Instance::new("sess-unseen", "/tmp/seed")];
        let prev = std::collections::HashMap::new();

        seed_tick_tracking(&mut fresh, &prev);

        assert!(!fresh[0].ever_confirmed_present);
        assert_eq!(fresh[0].unknown_since, None);
        assert_eq!(
            fresh[0].detection,
            crate::session::DetectionState::default()
        );
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// #3642.
    #[test]
    #[serial_test::serial]
    fn a_proposal_survives_the_tick_that_reloads_its_row_from_disk() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        // Never mutated.
        let mut on_disk = Instance::new("aoe_test_3642_tick", "/tmp");
        on_disk.status = Status::Running;
        assert_eq!(
            on_disk.tool, "claude",
            "fixture invariant: this test needs an agent with a manifest"
        );

        let session_name = crate::tmux::Session::generate_name(&on_disk.id, &on_disk.title);
        let _kill = crate::tmux::test_helpers::TmuxTestSession::from_name(session_name.clone());
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "40",
                "printf 'turn over\n'; sleep 300",
            ])
            .output()
            .expect("spawn tmux");
        assert!(
            created.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[session_name.as_str()]);

        let mut prev = std::collections::HashMap::from([(on_disk.id.clone(), Status::Running)]);
        let mut tracking: std::collections::HashMap<String, PriorTickTracking> =
            std::collections::HashMap::new();

        // One daemon tick, reporting the status it settled on and the rule that decided.
        let mut tick = |window_activity: Option<i64>| {
            let metadata = std::collections::HashMap::from([(
                session_name.clone(),
                crate::tmux::PaneMetadata {
                    pane_dead: false,
                    pane_current_command: Some("claude".to_string()),
                    pane_start_command_is_protected: false,
                    pane_pid: None,
                    pane_title: None,
                    window_activity,
                    window_size: None,
                },
            )]);
            let mut instances = vec![on_disk.clone()];
            seed_tick_tracking(&mut instances, &tracking);
            apply_tick_status_decisions(
                &mut instances,
                &prev,
                &std::collections::HashSet::new(),
                Some(&metadata),
            );
            tracking = instances
                .iter()
                .map(|i| (i.id.clone(), PriorTickTracking::of(i)))
                .collect();
            // A passive transition reaches disk in the tick that publishes it
            // (`flush_passive_transition_writes`), so the next tick's disk
            // load agrees with what this one decided.
            on_disk.status = instances[0].status;
            prev.insert(instances[0].id.clone(), instances[0].status);
            (instances[0].status, instances[0].detection.rule)
        };

        // No activity stamp.
        assert_eq!(
            tick(None).0,
            Status::Running,
            "an unwitnessed Idle waits for a tick that agrees with it"
        );
        assert_eq!(
            tick(None).0,
            Status::Idle,
            "the tick that agrees publishes it (#3642)"
        );

        // A stamp whose second is already past.
        let settled = Utc::now().timestamp() - 60;
        assert_eq!(tick(Some(settled)).0, Status::Idle);
        assert_eq!(
            tick(Some(settled)),
            (Status::Idle, Some("screen_unchanged")),
            "a skipped tick must leave the published status standing, not \
             re-derive one from a row it did not capture for"
        );
    }

    /// #1271: a cascade error string is carried only while the row is still in Error;
    /// any healthy fresh status drops it rather than propagating a stale message.
    #[test]
    fn merge_runtime_fields_carries_last_error_only_while_still_in_error() {
        let merged = |prior_status, fresh_status| {
            let mut prior = Instance::new("seed", "/tmp/seed");
            prior.status = prior_status;
            prior.last_error = Some("recovery cascade: foo".to_string());
            let mut fresh = Instance::new("seed", "/tmp/seed");
            fresh.status = fresh_status;
            fresh.last_error = None;
            merge_runtime_fields(prior, fresh).last_error
        };
        assert_eq!(
            merged(Status::Error, Status::Error).as_deref(),
            Some("recovery cascade: foo")
        );
        assert_eq!(merged(Status::Error, Status::Idle), None);
        assert_eq!(merged(Status::Idle, Status::Idle), None);
    }

    #[test]
    fn merge_runtime_fields_preserves_acp_load_session_capability() {
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.acp_load_session_capable = Some(true);

        let fresh = Instance::new("seed", "/tmp/seed");
        let merged = merge_runtime_fields(prior, fresh);

        assert_eq!(merged.acp_load_session_capable, Some(true));
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn reload_captures_publications_from_a_replaced_execution() {
        use crate::session::{ConversationProvenance, ExecutionBinding};
        use std::os::unix::fs::DirBuilderExt;

        let app = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(app.path());
        let file_watch = FileWatchService::new().unwrap();
        let hook_base = app.path().join("hooks");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&hook_base)
            .unwrap();
        let mergers: [fn(Instance, Instance) -> Instance; 2] =
            [merge_runtime_fields, |prior, mut fresh| {
                fresh.merge_runtime_from_reload(&prior);
                fresh
            }];
        for merge in mergers {
            let mut prior = Instance::new("reload-capture", app.path().to_str().unwrap());
            prior.tool = "claude".into();
            prior.source_profile = "reload-capture".into();
            prior.status = Status::Running;
            let hooks = hook_base.join(&prior.id);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&hooks)
                .unwrap();
            let launch = uuid::Uuid::new_v4().to_string();
            prior.active_execution = Some(
                serde_json::from_value(serde_json::json!({
                    "launch_id": launch,
                    "binding": ExecutionBinding {
                        agent: "claude".into(),
                        stores: vec![app.path().to_path_buf()],
                        configuration: Vec::new(),
                        exported_default_store: false,
                        cwd: app.path().to_path_buf(),
                        cwd_filesystem: "host".into(),
                        filesystem: "host".into(),
                    },
                    "capture": { "Hooks": hooks.join(format!("session_id.{launch}")) },
                    "container": null,
                }))
                .unwrap(),
            );
            assert_eq!(
                prior.maybe_start_poller(),
                crate::session::PollerStart::Started
            );
            let mut fresh: Instance =
                serde_json::from_str(&serde_json::to_string(&prior).unwrap()).unwrap();
            fresh.source_profile = prior.source_profile.clone();
            let launch = uuid::Uuid::new_v4().to_string();
            let publication = hooks.join(format!("session_id.{launch}"));
            let active = fresh.active_execution.as_mut().unwrap();
            active.launch_id = launch;
            active.capture =
                Some(serde_json::from_value(serde_json::json!({ "Hooks": publication })).unwrap());
            let storage = Storage::new_unwatched(&fresh.source_profile).unwrap();
            storage
                .update(|rows, _| {
                    *rows = vec![fresh.clone()];
                    Ok(())
                })
                .unwrap();
            let mut reloaded = merge(prior, fresh);
            let sid = uuid::Uuid::new_v4().to_string();
            std::fs::write(&publication, &sid).unwrap();
            let snapshot = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(vec![reloaded.tmux_session().unwrap().name().to_string()]),
                None,
            );
            reloaded.repair_session_id_poller_if_needed(&snapshot);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                crate::session::sync::drain_and_persist_session_ids(
                    std::slice::from_mut(&mut reloaded),
                    &file_watch,
                );
                if reloaded.agent_session_id.as_deref() == Some(&sid)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            reloaded.stop_poller();
            let stored = storage.load().unwrap().remove(0);
            assert_eq!(stored.agent_session_id.as_deref(), Some(sid.as_str()));
            assert_eq!(
                stored.agent_session_binding.unwrap().provenance,
                ConversationProvenance::Observed
            );
        }
    }
}

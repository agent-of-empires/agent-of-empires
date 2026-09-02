//! Applying what the deletion, stop, trash, restart, recovery, and
//! session-id pollers hand back.

use super::*;

impl HomeView {
    pub fn apply_deletion_results(&mut self) -> bool {
        use crate::session::deletion::DeletionDisposition;
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        match self.deletion_poller.try_recv_result() {
            Ok(result) => {
                match result.disposition {
                    DeletionDisposition::Removed | DeletionDisposition::AlreadyGone => {
                        self.instances.shift_remove(&result.session_id);
                        self.rebuild_group_trees();
                        self.rebuild_flat_items();
                    }
                    DeletionDisposition::KeptRestored => {
                        if let (Some(current), Some(retained)) = (
                            self.instances.get_mut(&result.session_id),
                            result.retained_instance,
                        ) {
                            current.lifecycle_generation = retained.lifecycle_generation;
                            current.status = retained.status;
                            current.trashed_at = retained.trashed_at;
                            current.project_path = retained.project_path;
                            current.pre_trash_project_path = retained.pre_trash_project_path;
                            current.lifecycle_reservation = retained.lifecycle_reservation;
                        }
                        let message = if result.teardown_started {
                            "This session was restored while its delete ran; the record was kept, but its worktree, branch, container, or transcript may already be gone. Inspect and repair it."
                        } else {
                            "This session is being restored by another process; it was not deleted."
                        };
                        self.info_dialog = Some(InfoDialog::new("Session restored", message));
                        self.rebuild_flat_items();
                    }
                    DeletionDisposition::Busy => {
                        if let Some(current) = self.instances.get_mut(&result.session_id) {
                            if let Some(retained) = result.retained_instance {
                                current.status = retained.status;
                                current.lifecycle_generation = retained.lifecycle_generation;
                                current.lifecycle_reservation = retained.lifecycle_reservation;
                            } else {
                                current.status = Status::Error;
                            }
                        }
                        self.info_dialog = Some(InfoDialog::new(
                            "Delete in progress",
                            "This session is already being deleted by another process.",
                        ));
                    }
                    DeletionDisposition::Failed => {
                        let error = if result.errors.is_empty() {
                            None
                        } else {
                            Some(result.errors.join("; "))
                        };
                        self.mutate_instance(&result.session_id, |inst| {
                            inst.status = Status::Error;
                            inst.last_error = error;
                        });
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                let stuck: Vec<String> = self
                    .instances
                    .values()
                    .filter(|instance| instance.status == Status::Deleting)
                    .map(|instance| instance.id.clone())
                    .collect();
                if stuck.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "deletion poller worker gone; marking stuck Deleting rows Error",
                );
                for id in &stuck {
                    self.mutate_instance(id, |inst| {
                        inst.status = Status::Error;
                        inst.last_error =
                            Some("Deletion worker crashed; session was not deleted".to_string());
                    });
                }
                true
            }
        }
    }

    /// Apply the result of a background stop. Returns true if an instance was
    /// updated so the caller can trigger a redraw.
    pub fn apply_stop_results(&mut self) -> bool {
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        match self.stop_poller.try_recv_result() {
            Ok(result) => {
                // `Instance::stop` committed its terminal state while holding
                // the cross-process lifecycle lock. Merge that durable row on
                // both success and failure so the in-memory error message is
                // attached to the generation that actually failed.
                let committed = self
                    .get_instance(&result.session_id)
                    .map(|instance| instance.source_profile.clone())
                    .and_then(|profile| self.storages.get(&profile))
                    .and_then(|storage| storage.load().ok())
                    .and_then(|instances| {
                        instances
                            .into_iter()
                            .find(|instance| instance.id == result.session_id)
                    });
                if let Some(committed) = committed {
                    self.mutate_instance(&result.session_id, |instance| {
                        instance.merge_post_start(&committed);
                    });
                }
                if !result.success {
                    self.set_instance_error(&result.session_id, result.error);
                    self.set_instance_status(&result.session_id, Status::Error);
                    if let Err(e) = self.save() {
                        tracing::error!(target: "tui.home", "Failed to save after stop: {}", e);
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The single worker thread is gone (a panic in perform_stop
                // dropped result_tx). Rows were optimistically marked Stopped
                // at request time and Stopped is frozen for the StatusPoller
                // (tier 0), so a lost failure result would otherwise show
                // "Stopped" over a still-running container forever; only the
                // poller's in-flight set knows which rows those are. Mirrors
                // the Disconnected handling in `apply_restart_results`.
                let stuck = self.stop_poller.take_pending();
                if stuck.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "stop poller worker gone; marking in-flight stops Error",
                );
                for id in &stuck {
                    self.set_instance_error(
                        id,
                        Some("Stop worker crashed; the session may not have stopped".to_string()),
                    );
                    self.set_instance_status(id, Status::Error);
                }
                if let Err(e) = self.save() {
                    tracing::error!(target: "tui.home", "Failed to save after stop: {}", e);
                }
                true
            }
        }
    }

    /// Apply a background trash result. The worker already committed durable
    /// state while holding the lifecycle flock; this drain only refreshes the
    /// in-memory path after confirming the same durable row is still trashed.
    pub fn apply_trash_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.trash_poller.try_recv_result() {
            Ok(result) => {
                let mut changed = false;
                if let Some(relocation) = result.relocation {
                    let durable = self
                        .instances
                        .get(&result.session_id)
                        .map(|instance| instance.source_profile.clone())
                        .and_then(|profile| self.storages.get(&profile))
                        .and_then(|storage| storage.load().ok())
                        .and_then(|instances| {
                            instances
                                .into_iter()
                                .find(|instance| instance.id == result.session_id)
                        });
                    if let Some(durable) = durable.filter(|instance| {
                        instance.is_trashed()
                            && instance.project_path == relocation.new_project_path
                    }) {
                        if let Some(instance) = self.instances.get_mut(&result.session_id) {
                            instance.project_path = durable.project_path;
                            instance.pre_trash_project_path = durable.pre_trash_project_path;
                            instance.lifecycle_generation = durable.lifecycle_generation;
                            instance.lifecycle_reservation = durable.lifecycle_reservation;
                            changed = true;
                        }
                    }
                }
                if let Some(reason) = result.relocate_warning {
                    tracing::warn!(
                        target: "tui.session",
                        session = %result.session_id,
                        "trash transition incomplete: {reason}",
                    );
                }
                changed
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                let stuck = self.trash_poller.take_pending();
                if stuck.is_empty() {
                    return false;
                }
                tracing::error!(
                    target: "tui.home",
                    rows = stuck.len(),
                    "trash poller worker gone; transitions recover after reservation expiry",
                );
                false
            }
        }
    }

    /// How long startup recovery waits for the first reconcile sweep before
    /// starting without it.
    ///
    /// The sweep takes milliseconds on a healthy store, but it writes through
    /// `Storage::update`, and `acquire_open_storage_flock` retries a contended
    /// profile lock forever with no timeout. A peer holding that lock would
    /// otherwise leave the worker neither delivering nor disconnecting, and
    /// recovery gated behind it for the whole boot. Recovering from a stale
    /// path is a wasted attempt; not recovering at all is a dead session.
    /// Gap between retries of a reconcile reload that failed, matching the
    /// heartbeat reload's own cadence in the same loop.
    pub(super) const RECONCILE_RELOAD_RETRY_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(5);

    pub(super) const STARTUP_RECOVERY_GATE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(30);

    /// Start startup auto-recovery once the first sweep has landed, or once
    /// [`Self::STARTUP_RECOVERY_GATE_TIMEOUT`] has elapsed if it never does.
    /// Idempotent: the gate is cleared as it fires.
    pub(super) fn release_startup_recovery_gate(&mut self, sweep_landed: bool) {
        // The gate exists so recovery reads repaired rows, and that holds only
        // if every repair has already been applied to `instances` when it
        // opens. The ordering is otherwise invisible from outside the call, so
        // it is asserted here rather than left to a test to notice.
        debug_assert!(
            !self.pending_reconcile_reload,
            "startup recovery gate released with a repair still unapplied",
        );
        let Some(armed_at) = self.startup_recovery_gate else {
            return;
        };
        if !sweep_landed {
            if armed_at.elapsed() < Self::STARTUP_RECOVERY_GATE_TIMEOUT {
                return;
            }
            tracing::warn!(
                target: "tui.home",
                "load-time reconciliation has not landed; starting startup recovery without it",
            );
        }
        self.startup_recovery_gate = None;
        self.maybe_start_startup_recovery();
    }

    /// Reload once the background load-time healing sweeps land, so a row the
    /// worker repointed is shown at its real path. Nothing to merge field by
    /// field: the sweeps only rewrite durable state, so a storage reload is
    /// both sufficient and cheaper than mirroring each repair. See #3611.
    pub fn apply_reconcile_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        // Probe before anything else, the deadline included. A repair sitting
        // in the channel has to reach `instances` before the gate opens, or
        // startup recovery clones a `project_path` the sweep has already fixed
        // on disk and spends that row's one boot-scoped attempt on it, which is
        // the failure the gate exists to prevent.
        let mut sweep_landed = self.pending_reconcile_reload;
        if !sweep_landed {
            match self.reconcile_poller.try_recv_result() {
                Ok(result) => {
                    sweep_landed = true;
                    self.pending_reconcile_reload = result.changed;
                }
                // The worker is gone, so neither a sweep nor a repair is
                // coming; nothing is left to apply before opening the gate.
                Err(TryRecvError::Disconnected) => sweep_landed = true,
                Err(TryRecvError::Empty) => {}
            }
        }

        // Only the reload waits on live-send, since it repaints. An unapplied
        // repair holds the gate shut regardless of the deadline: opening early
        // is worse than opening late. With nothing to apply the deadline still
        // runs, so a long paste cannot strand recovery on its own.
        if self.live_send.is_some() {
            if !self.pending_reconcile_reload {
                self.release_startup_recovery_gate(sweep_landed);
            }
            return false;
        }

        let mut reloaded = false;
        if self.pending_reconcile_reload {
            // Back off between attempts. This runs once per tick (~30Hz), and
            // the heartbeat reload beside it retries on a 5s interval, so an
            // unreadable store would otherwise spin on storage and emit tens of
            // warn lines a second where every other reload in the loop is
            // throttled.
            if self
                .reconcile_reload_retry_at
                .is_some_and(|at| std::time::Instant::now() < at)
            {
                return false;
            }
            match self.reload_storage_only() {
                Ok(()) => {
                    self.pending_reconcile_reload = false;
                    self.reconcile_reload_retry_at = None;
                    reloaded = true;
                }
                Err(error) => {
                    // The repair stays pending and the gate stays shut, so a
                    // later tick retries rather than letting recovery run
                    // against rows the sweep has already superseded on disk.
                    // The gate deliberately stays shut for the whole boot if
                    // this never succeeds: recovery reads the same store, so
                    // opening it would only spend each row's one attempt
                    // against the same failure.
                    tracing::warn!(
                        target: "tui.home",
                        "reload after load-time reconciliation failed: {error}",
                    );
                    self.reconcile_reload_retry_at =
                        Some(std::time::Instant::now() + Self::RECONCILE_RELOAD_RETRY_INTERVAL);
                    return false;
                }
            }
        }
        // Released only now, so recovery reads the repaired rows.
        self.release_startup_recovery_gate(sweep_landed);
        reloaded
    }

    /// Apply any pending session ID updates from background pollers.
    /// Returns true if any instance's in-memory `agent_session_id` changed.
    /// Tmux env may also be republished when this returns `false`
    /// (filtered or Failed paths republish the in-memory mirror).
    pub fn apply_session_id_updates(&mut self) -> bool {
        // Drain before repair: a poller can have one final queued observation
        // after its worker exits, and replacing it first would discard that
        // durable update.
        let mut changed = false;
        if self
            .instances
            .values()
            .any(|i| i.session_id_poller.is_some())
        {
            // `drain_and_persist_session_ids` takes `&mut [Instance]` and is
            // shared with `src/server/session_identity.rs`. Snapshot into a `Vec` at the
            // boundary, then re-`insert` touched ids back into the map;
            // `IndexMap::insert` on an existing key updates in place,
            // preserving position. The full-object re-insert is sound here
            // because the TUI event loop is single-threaded: nothing mutates
            // `self.instances` between this snapshot and the re-insert, so the
            // snapshot cannot go stale and clobber a concurrent field write.
            // The daemon holds `instances` under a shared async lock, so it
            // merges only the identity under a baseline CAS in
            // `apply_drained_identity_if_unchanged`; keep the two in sync.
            let mut snapshot: Vec<Instance> = self.cloned_instances();
            let outcome = crate::session::sync::drain_and_persist_session_ids(
                &mut snapshot,
                &self.file_watch,
            );
            if outcome.touched() {
                let touched: HashSet<&str> = outcome
                    .applied
                    .iter()
                    .chain(outcome.rolled_back.iter())
                    .map(String::as_str)
                    .collect();
                for inst in snapshot
                    .into_iter()
                    .filter(|i| touched.contains(i.id.as_str()))
                {
                    self.instances.insert(inst.id.clone(), inst);
                }
                changed = !outcome.applied.is_empty() || !outcome.rolled_back.is_empty();
            }
        }

        changed
    }

    /// Recreate stopped terminal session-id pollers after a status-refresh
    /// cadence has refreshed tmux state. This is deliberately separate from
    /// [`Self::apply_session_id_updates`], which runs on every input/render
    /// wake while live views are open.
    pub fn repair_session_id_pollers(&mut self) {
        // One observation for the whole walk. This runs on the `App::run` tick
        // over every instance, so a per-item `list-sessions` fork scales with
        // the store and lands on the thread that also serves keystrokes.
        // Profiling a store of a few hundred sessions put this path at the top
        // of the main thread.
        let live = crate::tmux::LiveSessionSnapshot::new();
        for instance in self.instances.values_mut() {
            instance.repair_session_id_poller_if_needed(&live);
        }
    }

    /// Drain the startup-recovery channel and apply each `RecoveryUpdate`
    /// to the in-memory `Instance` snapshot. Released the recovery lock
    /// (and the receiver) when all workers have completed.
    ///
    /// Called from the `App::run` event-loop tick alongside
    /// `apply_session_id_updates`. Returns true if any instance was
    /// touched, so the caller can refresh the rendered tree.
    pub fn apply_recovery_updates(&mut self) -> bool {
        let Some(rx) = self.recovery_rx.as_ref() else {
            return false;
        };
        let mut touched = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(update) => {
                    let RecoveryUpdate {
                        instance_id,
                        title,
                        instance,
                        result,
                    } = update;
                    match result {
                        Ok(crate::session::StartOutcome::Resumed) => {
                            tracing::info!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                "resumed",
                            );
                        }
                        Ok(crate::session::StartOutcome::ResumeFailed { sid }) => {
                            tracing::warn!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                %sid,
                                "resume failed; sid preserved for explicit retry",
                            );
                        }
                        Ok(crate::session::StartOutcome::Fresh) => {}
                        Ok(crate::session::StartOutcome::FreshAfterFailedResume { sid }) => {
                            // Defensive: `is_recovery_candidate` already excludes
                            // sids equal to `resume_probe_failed_sid`, so this
                            // should not normally fire here. See #2609.
                            tracing::info!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                %sid,
                                "started fresh; sid previously failed a resume probe",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "session.startup_recovery",
                                id = %instance_id,
                                %title,
                                error = %e,
                                "recovery cascade failed",
                            );
                        }
                    }
                    // Drop the in-flight marker BEFORE replacing the
                    // snapshot so the next status poll sees the post-cascade
                    // instance through the normal pipeline.
                    self.recovery_in_flight.remove(&instance_id);
                    if let Some(slot) = self.instances.get_mut(&instance_id) {
                        *slot = *instance;
                        touched = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            // All workers exited: drop the receiver and the lock so a
            // peer (a daemon that just started) can run recovery for any
            // session this TUI did not own. Clear the in-flight set
            // defensively in case a future early-return path bypassed
            // the per-id remove above.
            self.recovery_rx = None;
            self.recovery_lock = None;
            self.recovery_in_flight.clear();
        }
        if touched {
            self.refresh_rows_preserving_selection();
        }
        touched
    }

    /// Rebuild `flat_items` after a background worker replaced an `Instance`
    /// snapshot, preserving the current selection. Without the
    /// selection restore, a completion that reorders rows (e.g. a shifted
    /// `last_start_time` under `SortOrder::LastActivity`) would silently latch
    /// the cursor onto a neighbour, since `update_selected()` resolves through
    /// `flat_items[cursor]`. Mirrors the canonical sequence in `reload()`.
    /// Shared by `apply_recovery_updates` and `apply_restart_results`.
    fn refresh_rows_preserving_selection(&mut self) {
        let prev_selected_session = self.selected_session.clone();
        let prev_selected_group = self.selected_group.clone();

        self.rebuild_flat_items();

        let mut restored = false;
        if let Some(ref sid) = prev_selected_session {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Session { id, .. } = item {
                    if id == sid {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        } else if let Some(ref gpath) = prev_selected_group {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Group { path, .. } = item {
                    if path == gpath {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        }
        if !restored && self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() {
            self.cursor = self.flat_items.len() - 1;
        }

        if self.search_active && !self.search_query.value().is_empty() {
            self.update_search();
        } else if !self.search_matches.is_empty() {
            self.refresh_search_matches();
        }

        self.update_selected();
    }

    /// Apply results from the restart poller. Writes the post-cascade `Instance`
    /// snapshot back into memory (so `restart_with_size`'s mutations and the
    /// `#[serde(skip)]` `last_start_time` survive), clears the in-flight marker,
    /// and persists. A failed cascade or preserved resume-probe failure surfaces
    /// as a "Restart Failed" dialog (the user explicitly initiated the restart).
    /// Returns true if any instance changed.
    pub fn apply_restart_results(&mut self) -> bool {
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        let mut touched = false;
        loop {
            match self.restart_poller.try_recv_result() {
                Ok(result) => {
                    let crate::session::restart::RestartResult {
                        session_id,
                        before,
                        mut instance,
                        outcome,
                    } = result;

                    self.restart_in_flight.remove(&session_id);

                    match outcome {
                        Ok(crate::session::StartOutcome::ResumeFailed { sid }) => {
                            tracing::warn!(
                                target: "session.restart",
                                id = %session_id,
                                %sid,
                                "resume failed; sid preserved for explicit retry",
                            );
                            self.info_dialog = Some(InfoDialog::new(
                                "Restart Failed",
                                &format!(
                                    "Resume failed for sid {sid}; preserved for explicit retry"
                                ),
                            ));
                        }
                        Ok(crate::session::StartOutcome::FreshAfterFailedResume { sid }) => {
                            tracing::info!(
                                target: "session.restart",
                                id = %session_id,
                                %sid,
                                "started fresh; sid previously failed a resume probe",
                            );
                            self.info_dialog = Some(InfoDialog::new(
                                "Restarted",
                                &format!(
                                    "Started fresh; a prior resume attempt failed for sid {sid}. \
                                     The old conversation is still reachable via the agent's \
                                     own resume/history picker."
                                ),
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                target: "session.restart",
                                id = %session_id,
                                error = %e,
                                "restart cascade failed",
                            );
                            instance.status = Status::Error;
                            instance.last_error = Some(e.clone());
                            // Surface it: a cascade failure now arrives async, so
                            // the input handler's "Restart Failed" dialog can no
                            // longer catch it (restart_selected_session returned
                            // Ok once the work was enqueued).
                            self.info_dialog = Some(InfoDialog::new(
                                "Restart Failed",
                                &format!("Could not restart session: {e}"),
                            ));
                        }
                    }

                    if let Some(slot) = self.instances.get_mut(&session_id) {
                        slot.merge_post_restart_with_baseline(&before, &instance);
                        slot.last_error = if instance.status == Status::Error {
                            instance.last_error.clone()
                        } else {
                            None
                        };
                        slot.last_error_check = instance.last_error_check;
                        slot.last_start_time = instance.last_start_time;
                        slot.retroactive_capture_excludes =
                            instance.retroactive_capture_excludes.clone();
                        touched = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The single worker thread is gone (a panic in
                    // perform_restart dropped result_tx). Clear the in-flight set
                    // defensively so the stuck rows fall back to the StatusPoller
                    // (which marks them Error) instead of being filtered out of
                    // polling forever by `pollable_instances`. Mirrors the
                    // Disconnected handling in `apply_recovery_updates`.
                    if !self.restart_in_flight.is_empty() {
                        tracing::error!(
                            target: "session.restart",
                            "restart poller worker gone; clearing in-flight set",
                        );
                        self.restart_in_flight.clear();
                        touched = true;
                    }
                    break;
                }
            }
        }

        if touched {
            self.refresh_rows_preserving_selection();
            if let Err(e) = self.save() {
                tracing::error!(target: "tui.home", "Failed to save after restart: {}", e);
            }
        }
        touched
    }

    /// Identify recovery candidates and spawn a worker pool. Sets
    /// `self.recovery_rx` to `Some(rx)` if at least one worker was spawned;
    /// otherwise leaves it `None` (the daemon owns recovery, the lock is
    /// contended, or there are no candidates).
    pub(super) fn maybe_start_startup_recovery(&mut self) {
        // Requires a tokio runtime: each worker is `tokio::spawn`-ed below.
        // `HomeView::new` is sync and called from production via
        // `#[tokio::main]`, so the runtime is present at the real call site.
        // Unit tests construct `HomeView` directly without a runtime; today
        // they do not panic only because their test instances lack a valid
        // `agent_session_id` and `is_recovery_candidate` filters them out
        // before any spawn is attempted. This guard makes the function
        // resilient to a future test that constructs an instance with a
        // valid sid and no live tmux.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // Defer to the daemon if one is running. The daemon's own
        // `daemon_startup_recovery` will handle the candidates from this
        // TUI's profile (and every other profile). Recovery split-brain
        // is the exact failure mode the file lock is meant to prevent;
        // checking `daemon_pid()` first short-circuits the more expensive
        // lock acquisition in the common case.
        if crate::cli::serve::daemon_pid().is_some() {
            return;
        }
        let lock = match crate::session::recovery::try_acquire_recovery_lock() {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::info!(
                    target: "session.startup_recovery",
                    "another process holds the recovery lock; TUI skipping startup recovery",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "session.startup_recovery",
                    error = %e,
                    "failed to acquire recovery lock; TUI skipping startup recovery",
                );
                return;
            }
        };

        let mut candidates: Vec<crate::session::Instance> = Vec::new();
        // Single fallible tmux probe instead of a per-instance liveness
        // lookup. On Err: skip recovery this launch (a transient tmux glitch
        // must NOT collapse to "all panes dead" and trigger phantom
        // cascades). Bonus: one subprocess call regardless of instance count
        // (was 1-2 per instance).
        let pane_meta = match crate::tmux::batch_pane_metadata() {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    target: "session.startup_recovery",
                    error = %e,
                    "tmux probe failed; TUI skipping startup recovery this launch",
                );
                drop(lock);
                return;
            }
        };
        // Pass 1: eligible = missing tmux pane + recovery candidate + not
        // already attempted this boot. The boot-scoped ledger (#2994) makes
        // startup recovery idempotent per boot for every agent, so a session a
        // prior pass already resumed (before its owner exited) is never
        // recreated here.
        let attempted = crate::session::recovery::recovery_attempted_this_boot();
        let eligible: Vec<crate::session::Instance> = self
            .instances
            .values()
            .filter(|inst| {
                let session_name = crate::tmux::resolve_agent_session_name_in(
                    &pane_meta,
                    &inst.id,
                    &crate::tmux::Session::generate_name(&inst.id, &inst.title),
                );
                let has_live_tmux = pane_meta
                    .get(&session_name)
                    .map(|m| !m.pane_dead)
                    .unwrap_or(false);
                !has_live_tmux
                    && crate::session::recovery::is_recovery_candidate(inst)
                    && !attempted.contains(&inst.id)
            })
            .cloned()
            .collect();

        // #2994 (defense-in-depth): one batched process-table walk drops any
        // session whose agent is positively still alive on a tmux server this
        // process can no longer see (its socket dir was wiped mid-crash).
        let orphan_flags = crate::session::recovery::orphaned_agents_alive(&eligible);

        // Pass 2: commit the survivors.
        for (idx, elig) in eligible.iter().enumerate() {
            if orphan_flags.get(idx).copied().unwrap_or(false) {
                tracing::info!(
                    target: "session.startup_recovery",
                    id = %elig.id,
                    "skipping recovery: agent already alive on an orphaned tmux server",
                );
                continue;
            }
            if let Some(inst) = self.instances.get_mut(&elig.id) {
                // Set Status::Starting AND last_start_time: the existing 3s
                // grace at `update_status_with_metadata_inner` only fires on
                // the latter, and without it the TUI's StatusPoller (every
                // 500ms) would observe missing tmux + no last_start_time and
                // immediately flip the status to `Error` before the worker
                // has finished its cascade.
                debug_assert!(inst.status != crate::session::Status::Creating);
                inst.status = crate::session::Status::Starting;
                inst.last_error = None;
                inst.last_start_time = Some(std::time::Instant::now());
                self.recovery_in_flight.insert(inst.id.clone());
                candidates.push(inst.clone());
            }
        }

        if candidates.is_empty() {
            drop(lock);
            return;
        }

        // Record the attempt before any worker runs `tmux new-session`, so a
        // mid-pass crash fails toward "already attempted" for the next pass.
        crate::session::recovery::mark_recovery_attempted(
            &candidates.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
        );

        crate::session::recovery::warm_tmux_server();

        tracing::info!(
            target: "session.startup_recovery",
            count = candidates.len(),
            "TUI starting recovery for missing tmux sessions",
        );

        let (tx, rx) = std::sync::mpsc::channel::<RecoveryUpdate>();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::session::recovery::STARTUP_RECOVERY_CONCURRENCY,
        ));

        for inst in candidates {
            let tx = tx.clone();
            let permit_sem = semaphore.clone();
            tokio::spawn(async move {
                let _permit = permit_sem
                    .acquire_owned()
                    .await
                    .expect("recovery semaphore not closed");
                let id = inst.id.clone();
                let title = inst.title.clone();
                let inst_pre_panic = inst.clone();
                let mut working = inst;
                let result = tokio::task::spawn_blocking(move || {
                    let res = crate::session::recovery::run_recovery_for_instance(&mut working);
                    (working, res)
                })
                .await;
                let update = match result {
                    Ok((updated, Ok(outcome))) => RecoveryUpdate {
                        instance_id: id,
                        title,
                        instance: Box::new(updated),
                        result: Ok(outcome),
                    },
                    Ok((updated, Err(e))) => RecoveryUpdate {
                        instance_id: id,
                        title,
                        instance: Box::new(updated),
                        result: Err(e.to_string()),
                    },
                    Err(join_err) => {
                        tracing::error!(
                            target: "session.startup_recovery",
                            id = %id,
                            error = %join_err,
                            "recovery worker panicked",
                        );
                        // Surface the panic as a synthetic error update so
                        // `apply_recovery_updates` clears `recovery_in_flight`
                        // and the user sees Status::Error with a useful
                        // last_error instead of an instance stuck in
                        // `Status::Starting` until HomeView drops.
                        let mut recovered = inst_pre_panic;
                        recovered.status = crate::session::Status::Error;
                        recovered.last_error =
                            Some(format!("recovery worker panicked: {}", join_err));
                        RecoveryUpdate {
                            instance_id: id,
                            title,
                            instance: Box::new(recovered),
                            result: Err(format!("worker panicked: {}", join_err)),
                        }
                    }
                };
                let _ = tx.send(update);
            });
        }

        self.recovery_rx = Some(rx);
        self.recovery_lock = Some(lock);
    }
}

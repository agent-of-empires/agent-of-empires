//! Applying what the deletion, stop, trash, restart, recovery, and session-id pollers hand back.

use super::*;

impl HomeView {
    pub fn apply_deletion_results(&mut self) -> bool {
        use crate::session::deletion::DeletionDisposition;
        use crate::session::Status;

        match self.deletion_poller.try_recv_result() {
            Some(Ok(done)) => {
                let Some(pending) = self.deletes_in_flight.remove(&done.request_id) else {
                    return false;
                };
                let result = done.result;
                if result.session_id != pending.session_id {
                    return true;
                }
                let current_matches =
                    self.instances
                        .get(&pending.session_id)
                        .is_some_and(|current| {
                            pending.matches(current)
                                || (pending.created_at == current.created_at
                                    && current.storage_origin.as_ref().is_some_and(|storage| {
                                        pending.origin.storage.same_origin_as(storage)
                                    })
                                    && result.retained_release_matches(current))
                        });
                if !current_matches {
                    return true;
                }
                let still_pending = self
                    .deletes_in_flight
                    .values()
                    .any(|other| other.session_id == pending.session_id);
                if still_pending
                    && matches!(
                        result.disposition,
                        DeletionDisposition::Failed | DeletionDisposition::Busy
                    )
                {
                    let details = if result.errors.is_empty() {
                        "The original delete is still pending; this request did not complete."
                            .to_string()
                    } else {
                        result.errors.join("; ")
                    };
                    self.info_dialog = Some(InfoDialog::new("Delete did not complete", &details));
                    return true;
                }
                if result.disposition == DeletionDisposition::Failed {
                    self.failed_deletes
                        .insert(result.session_id.clone(), pending.attempt);
                } else {
                    self.failed_deletes.remove(&result.session_id);
                }
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
                        if result.retained_instance.is_some()
                            && !self
                                .instances
                                .get(&result.session_id)
                                .is_some_and(|inst| result.retained_release_matches(inst))
                        {
                            return true;
                        }
                        let error = if result.errors.is_empty() {
                            None
                        } else {
                            Some(result.errors.join("; "))
                        };
                        self.mutate_instance(&result.session_id, |inst| {
                            if let Some(retained) = result.retained_instance {
                                inst.lifecycle_generation = retained.lifecycle_generation;
                                inst.lifecycle_reservation = retained.lifecycle_reservation;
                                inst.runner_journal = retained.runner_journal;
                            }
                            inst.status = Status::Error;
                            inst.last_error = error;
                        });
                    }
                }
                true
            }
            None => false,
            Some(Err(ids)) => {
                let mut stuck = Vec::new();
                for request_id in ids {
                    let Some(pending) = self.deletes_in_flight.remove(&request_id) else {
                        continue;
                    };
                    if !self
                        .deletes_in_flight
                        .values()
                        .any(|other| other.session_id == pending.session_id)
                        && self.instances.get(&pending.session_id).is_some_and(|row| {
                            pending.matches(row) && row.status == Status::Deleting
                        })
                    {
                        stuck.push(pending.session_id);
                    }
                }
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

    pub fn apply_settlement_results(&mut self) -> bool {
        use crate::tui::stop_poller::{SettledEdit, SettlementAction};
        use std::sync::mpsc::TryRecvError;
        match self.settlement_poller.try_recv() {
            Ok(result) => {
                let request = result.request;
                if !super::RequestOrigin::retire(&mut self.settlement_in_flight, &request.instance)
                {
                    return false;
                }
                let current_matches =
                    self.instances
                        .get(&request.session_id)
                        .is_some_and(|current| {
                            current.same_storage_origin(&request.instance)
                                && (current.lifecycle_generation
                                    == request.instance.lifecycle_generation
                                    || result.generation.as_ref().is_ok_and(|custody| {
                                        current.lifecycle_generation == custody.stop.generation()
                                    }))
                                && current.active_execution == request.instance.active_execution
                        });
                if !current_matches || request.storage.verify_profile_identity().is_err() {
                    return true;
                }
                let custody = match result.generation {
                    Ok(generation) => generation,
                    Err(error) => {
                        self.info_dialog = Some(super::InfoDialog::new(
                            "Runner Settlement Pending",
                            &format!("{error:#}"),
                        ));
                        return true;
                    }
                };
                self.settled_edit = Some(SettledEdit {
                    storage: request.storage,
                    custody,
                });
                let outcome = match request.action {
                    SettlementAction::Workdir {
                        name,
                        rename_branch,
                    } => self.set_worktree_name_by_id(&request.session_id, &name, rename_branch),
                    SettlementAction::Rename {
                        title,
                        group,
                        profile,
                        rename_branch,
                    } => self.rename_session_by_id(
                        &request.session_id,
                        &title,
                        group.as_deref(),
                        profile.as_deref(),
                        rename_branch,
                    ),
                    SettlementAction::Archive { reveal } => {
                        let row = self.capture_transaction_row(&request.session_id);
                        match row {
                            Ok(row) => {
                                let settled = self.settled_edit.take();
                                let successor = self.archive_successor_session(&request.session_id);
                                self.request_transaction(
                                    persistence_transactions::TransactionRequest::Archive {
                                        row,
                                        settled,
                                        reveal,
                                        successor,
                                    },
                                )
                            }
                            Err(error) => Err(error),
                        }
                    }
                };
                self.settled_edit = None;
                if let Err(error) = outcome {
                    self.info_dialog = Some(super::InfoDialog::new(
                        "Session Edit Failed",
                        &format!("{error:#}"),
                    ));
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                let pending = self.settlement_poller.take_pending();
                if pending.is_empty() {
                    return false;
                }
                for id in pending {
                    self.settlement_in_flight.remove(&id);
                }
                self.info_dialog = Some(super::InfoDialog::new(
                    "Runner Settlement Pending",
                    "The settlement worker exited; no checkout move or archive was published",
                ));
                true
            }
        }
    }
    pub fn apply_stop_results(&mut self) -> bool {
        use crate::session::Status;
        use std::sync::mpsc::TryRecvError;

        match self.stop_poller.try_recv_result() {
            Ok(result) => {
                self.request_reload(super::ReloadKind::Full);
                if !result.success {
                    self.info_dialog = Some(InfoDialog::new(
                        "Stop Failed",
                        result
                            .error
                            .as_deref()
                            .unwrap_or("The original stop was not acknowledged"),
                    ));
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
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
                self.request_save();
                true
            }
        }
    }

    pub fn apply_trash_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.trash_poller.try_recv_result() {
            Ok(result) => {
                self.request_reload(super::ReloadKind::Full);
                if let Some(reason) = result.relocate_warning {
                    tracing::warn!(target: "tui.session", session = %result.session_id, "trash transition incomplete: {reason}");
                }
                true
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

    pub(super) const RECONCILE_RELOAD_RETRY_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(5);

    /// How long startup recovery waits for the first reconcile sweep, which can block on a contended profile lock.
    pub(super) const STARTUP_RECOVERY_GATE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(30);

    /// Start startup recovery once the first sweep landed, or after the gate timeout.
    pub(super) fn release_startup_recovery_gate(&mut self, sweep_landed: bool) {
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

    /// Reload once load-time healing lands. A pending repair keeps the recovery gate shut,
    /// so recovery never runs against rows the sweep already fixed on disk.
    pub fn apply_reconcile_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let mut sweep_landed = self.pending_reconcile_reload;
        if !sweep_landed {
            match self.reconcile_poller.try_recv_result() {
                Ok(result) => {
                    sweep_landed = true;
                    self.pending_reconcile_reload = result.changed;
                }
                Err(TryRecvError::Disconnected) => sweep_landed = true,
                Err(TryRecvError::Empty) => {}
            }
        }

        if self.live_send.is_some() {
            if !self.pending_reconcile_reload {
                self.release_startup_recovery_gate(sweep_landed);
            }
            return false;
        }

        if self.pending_reconcile_reload {
            if self
                .reconcile_reload_retry_at
                .is_some_and(|at| std::time::Instant::now() < at)
            {
                return false;
            }
            if !self.reconcile_reload_is_pending() {
                self.request_reload(super::ReloadKind::Reconciled);
            }
            return false;
        }
        self.release_startup_recovery_gate(sweep_landed);
        false
    }

    pub fn apply_session_id_updates(&mut self) -> bool {
        if !self
            .instances
            .values()
            .any(|i| i.session_id_poller.is_some())
        {
            return false;
        }
        // Whole-object re-insert is safe: the TUI loop is single-threaded, so the snapshot can't go stale.
        let mut snapshot: Vec<Instance> = self.cloned_instances();
        let outcome =
            crate::session::sync::drain_and_persist_session_ids(&mut snapshot, &self.file_watch);
        if !outcome.touched() {
            return false;
        }
        let touched: HashSet<&str> = outcome
            .applied
            .iter()
            .chain(outcome.rolled_back.iter())
            .chain(outcome.lifecycle_advanced.iter())
            .map(String::as_str)
            .collect();
        for inst in snapshot
            .into_iter()
            .filter(|i| touched.contains(i.id.as_str()))
        {
            self.instances.insert(inst.id.clone(), inst);
        }
        !outcome.applied.is_empty()
            || !outcome.rolled_back.is_empty()
            || !outcome.lifecycle_advanced.is_empty()
    }

    pub fn repair_session_id_pollers(&mut self) {
        let live = crate::tmux::LiveSessionSnapshot::new();
        for instance in self.instances.values_mut() {
            instance.repair_session_id_poller_if_needed(&live);
        }
    }

    pub fn apply_recovery_updates(&mut self) -> bool {
        if self.recovery_rx.is_none() {
            return false;
        }
        let mut touched = false;
        let mut disconnected = false;
        loop {
            match self
                .recovery_rx
                .as_ref()
                .expect("recovery receiver retained during drain")
                .try_recv()
            {
                Ok(update) => {
                    if !super::RequestOrigin::retire(&mut self.recovery_in_flight, &update.before) {
                        continue;
                    }
                    touched = true;
                    self.request_reload_after(
                        super::ReloadKind::Full,
                        persistence_lane::ReloadContinuation::Recovery(update),
                    );
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.recovery_rx = None;
            self.recovery_lock = None;
            self.recovery_in_flight.clear();
        }
        if touched {
            self.refresh_rows_preserving_selection();
        }
        touched
    }

    /// Rebuild rows after a worker replaced an instance, keeping the cursor on the same item.
    fn refresh_rows_preserving_selection(&mut self) {
        self.rebuild_flat_items_keeping_cursor();
        if self.search_active && !self.search_query.value().is_empty() {
            self.update_search();
        } else if !self.search_matches.is_empty() {
            self.refresh_search_matches();
        }

        self.update_selected();
    }

    pub fn apply_restart_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let mut touched = false;
        loop {
            match self.restart_poller.try_recv_result() {
                Ok(result) => {
                    let crate::session::restart::RestartResult {
                        session_id,
                        before,
                        instance,
                        outcome,
                    } = result;

                    if !super::RequestOrigin::retire(&mut self.restart_in_flight, &before) {
                        continue;
                    }
                    let attach_after = self.attach_after_restart.remove(&session_id);
                    touched = true;
                    self.request_reload_after(
                        super::ReloadKind::Full,
                        persistence_lane::ReloadContinuation::Restart {
                            result: crate::session::restart::RestartResult {
                                session_id,
                                before,
                                instance,
                                outcome,
                            },
                            attach_after,
                        },
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.restart_in_flight.is_empty() {
                        tracing::error!(
                            target: "session.restart",
                            "restart poller worker gone; clearing in-flight set",
                        );
                        self.restart_in_flight.clear();
                        self.attach_after_restart.clear();
                        touched = true;
                    }
                    break;
                }
            }
        }

        if touched {
            self.refresh_rows_preserving_selection();
        }
        touched
    }

    pub fn take_restarted_attaches(&mut self) -> Vec<String> {
        std::mem::take(&mut self.restarted_attaches)
    }

    pub(super) fn maybe_start_startup_recovery(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
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

        let pane_meta = match crate::tmux::batch_pane_metadata() {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    target: "session.startup_recovery",
                    error = %e,
                    "tmux probe failed; TUI skipping startup recovery this launch",
                );
                return;
            }
        };
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

        let orphan_flags = crate::session::recovery::orphaned_agents_alive(&eligible);
        let mut candidates = Vec::new();
        for (idx, elig) in eligible.iter().enumerate() {
            if orphan_flags.get(idx).copied().unwrap_or(false) {
                tracing::info!(
                    target: "session.startup_recovery",
                    id = %elig.id,
                    "skipping recovery: agent already alive on an orphaned tmux server",
                );
                continue;
            }
            let origin = match super::RequestOrigin::capture(elig) {
                Ok(origin) => origin,
                Err(error) => {
                    tracing::warn!(target: "session.startup_recovery", id = %elig.id, %error, "recovery authority unavailable");
                    self.info_dialog = Some(InfoDialog::new(
                        "Recovery not started",
                        &format!("{}: {error}", elig.title),
                    ));
                    continue;
                }
            };
            if let Some(inst) = self.instances.get_mut(&elig.id) {
                debug_assert!(inst.status != crate::session::Status::Creating);
                // `last_start_time` arms the status poller's startup grace; without it the row flips to Error.
                inst.status = crate::session::Status::Starting;
                inst.last_error = None;
                inst.last_start_time = Some(std::time::Instant::now());
                self.recovery_in_flight.insert(inst.id.clone(), origin);
                candidates.push(inst.clone());
            }
        }

        if candidates.is_empty() {
            return;
        }

        // Recorded before any worker runs, so a mid-pass crash counts as attempted.
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
                let (instance, result) = match result {
                    Ok((updated, res)) => (updated, res.map_err(|e| e.to_string())),
                    Err(join_err) => {
                        tracing::error!(
                            target: "session.startup_recovery",
                            id = %id,
                            error = %join_err,
                            "recovery worker panicked",
                        );
                        // Report the panic as an error so the row leaves Starting.
                        let mut recovered = inst_pre_panic.clone();
                        recovered.status = crate::session::Status::Error;
                        recovered.last_error =
                            Some(format!("recovery worker panicked: {}", join_err));
                        (recovered, Err(format!("worker panicked: {}", join_err)))
                    }
                };
                let _ = tx.send(RecoveryUpdate {
                    instance_id: id,
                    title,
                    before: Box::new(inst_pre_panic),
                    instance: Box::new(instance),
                    result,
                });
            });
        }

        self.recovery_rx = Some(rx);
        self.recovery_lock = Some(lock);
    }
    pub(super) fn apply_completed_recovery(&mut self, update: RecoveryUpdate) {
        let RecoveryUpdate {
            instance_id,
            title,
            before,
            instance,
            result,
        } = update;
        if !self.completed_launch_projection_matches(&before, &instance) {
            self.info_dialog = Some(InfoDialog::new("Recovery result rejected", "The original producer result no longer matches the acknowledged row. No replacement was changed."));
            return;
        }
        match result {
            Ok(crate::session::StartOutcome::Resumed) => {
                tracing::info!(target: "session.startup_recovery", id = %instance_id, %title, "resumed");
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
        if let Some(slot) = self.instances.get_mut(&instance_id) {
            slot.merge_post_restart_with_baseline(&before, &instance);
            slot.last_error = instance.last_error.clone();
            slot.last_error_check = instance.last_error_check;
            slot.last_start_time = instance.last_start_time;
        }
        self.refresh_rows_preserving_selection();
    }

    pub(super) fn apply_completed_restart(
        &mut self,
        result: crate::session::restart::RestartResult,
        attach_after: bool,
    ) {
        use crate::session::Status;
        let crate::session::restart::RestartResult {
            session_id,
            before,
            mut instance,
            outcome,
        } = result;
        if !self.completed_launch_projection_matches(&before, &instance) {
            self.info_dialog = Some(InfoDialog::new("Restart result rejected", "The original producer result no longer matches the acknowledged row. No replacement was changed or attached."));
            return;
        }
        if attach_after && crate::session::restart::launched_agent(&outcome) {
            self.restarted_attaches.push(session_id.clone());
        }

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
                    &format!("Resume failed for sid {sid}; preserved for explicit retry"),
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
            slot.retroactive_capture_excludes = instance.retroactive_capture_excludes.clone();
        }
        self.refresh_rows_preserving_selection();
        self.request_save();
    }

    fn completed_launch_projection_matches(&self, before: &Instance, after: &Instance) -> bool {
        before.created_at == after.created_at
            && before.same_storage_origin(after)
            && self.instances.get(&before.id).is_some_and(|current| {
                current.created_at == before.created_at
                    && current.same_storage_origin(before)
                    && current.lifecycle_generation == after.lifecycle_generation
                    && current.active_execution == after.active_execution
                    && current.tool == before.tool
            })
    }
}

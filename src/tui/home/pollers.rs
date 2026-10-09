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

#[cfg(all(test, debug_assertions, unix))]
mod hosted_tests {
    use super::*;
    use crate::acp::control_protocol::{self, ControlBody};
    use crate::runner_tests::{environment::EnvGuard, runner_fixture::RunnerLaunchFixture, shim};
    use crate::session::{LifecycleOperation, View, WorktreeInfo};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::time::{Duration, Instant};

    struct RunnerChild(std::process::Child);
    impl Drop for RunnerChild {
        fn drop(&mut self) {
            if self.0.try_wait().is_ok_and(|status| status.is_none()) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn screen(view: &mut HomeView) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                view.render(
                    frame,
                    frame.area(),
                    &crate::tui::styles::Theme::default(),
                    None,
                    None,
                    None,
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    #[ignore = "disposable hosted native producer proof only"]
    async fn hosted_force_refusal_dialog_keeps_original_purge_pending() {
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        assert_eq!(
            std::env::var("RUNNER_ENVIRONMENT").as_deref(),
            Ok("github-hosted")
        );
        assert!(matches!(
            std::env::var("RUNNER_OS").as_deref(),
            Ok("Linux" | "macOS")
        ));
        if !crate::runner_tests::isolated_case(
            module_path!(),
            stringify!(hosted_force_refusal_dialog_keeps_original_purge_pending),
        ) {
            return;
        }
        shim::shim_ready().expect("hosted SDK shim prerequisites");
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let home = root.path().join("home");
        let xdg = root.path().join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let _env = EnvGuard::new(&["HOME", "XDG_CONFIG_HOME"])
            .and_set("HOME", &home)
            .and_set("XDG_CONFIG_HOME", &xdg);
        crate::session::get_app_dir().unwrap();
        crate::migrations::run_migrations().unwrap();
        let id = "pending-force-ui";
        let checkout = crate::session::scratch::provision_scratch_dir(id).unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("owned-file"), "original resource").unwrap();
        git(&repo, &["add", "owned-file"]);
        git(&repo, &["commit", "-m", "seed"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "original",
                checkout.to_str().unwrap(),
            ],
        );
        let sentinel = checkout.join("owned-file");

        let hook_socket = root.path().join("hook.sock");
        let hook_done = root.path().join("hook.done");
        let hook_script = root.path().join("destroy.cjs");
        let release = uuid::Uuid::new_v4().to_string();
        let listener = UnixListener::bind(&hook_socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::write(
            &hook_script,
            format!(
                r#"
const net = require('node:net');
const fs = require('node:fs');
const socket = net.connect({socket});
socket.on('error', error => {{ throw error; }});
socket.on('connect', () => socket.write(JSON.stringify({{
  id: process.env.AOE_SESSION_ID, cwd: process.env.AOE_PROJECT_PATH, pid: process.pid
}}) + '\n'));
let received = '';
socket.on('data', bytes => {{
  received += bytes.toString();
  if (received === {release} + '\n') {{
    fs.appendFileSync({done}, process.env.AOE_SESSION_ID + '\n');
    socket.end();
  }}
}});
"#,
                socket = serde_json::to_string(&hook_socket).unwrap(),
                release = serde_json::to_string(&release).unwrap(),
                done = serde_json::to_string(&hook_done).unwrap(),
            ),
        )
        .unwrap();
        let hook_command = format!(
            "'{}' '{}'",
            shim::shim_node().unwrap().display(),
            hook_script.display()
        );
        crate::session::config::update_config(|config| {
            config.hooks.on_destroy = vec![hook_command];
        })
        .unwrap();
        let mut row = Instance::new("pending original", checkout.to_str().unwrap());
        row.id = id.into();
        row.source_profile = "main".into();
        row.view = View::Structured;
        row.scratch = true;
        row.worktree_info = Some(WorktreeInfo {
            branch: "original".into(),
            main_repo_path: repo.to_str().unwrap().into(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: Some("main".into()),
        });
        Storage::new_unwatched("main")
            .unwrap()
            .update(|rows, _| {
                rows.push(row);
                Ok(())
            })
            .unwrap();
        let fixture = RunnerLaunchFixture::new(&home, &xdg, "main", id);
        let socket = root.path().join("runner.sock");
        let control_socket = socket.with_extension("control.sock");
        let retire_gate = root.path().join("retire.release");
        let entered = retire_gate.with_extension("entered");
        let counter = root.path().join("native-starts");
        let mut command = fixture.command();
        command
            .args([
                "--socket",
                socket.to_str().unwrap(),
                "--session-id",
                id,
                "--agent-name",
                "shim",
                "--cwd",
                checkout.to_str().unwrap(),
                "--",
                shim::shim_node().unwrap().to_str().unwrap(),
                shim::shim_path().to_str().unwrap(),
            ])
            .env("AOE_TEST_STOP_RETIRE_GATE", &retire_gate)
            .env("SHIM_ENV_RECORD_FILE", &counter);
        let mut child = RunnerChild(
            fixture
                .spawn(&mut command)
                .expect("original managed SDK bootstrap"),
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !control_socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut attachment = tokio::net::UnixStream::connect(&control_socket)
                .await
                .unwrap();
            assert!(matches!(
                control_protocol::read_frame(&mut attachment).await.unwrap(),
                Some(ControlBody::Hello { .. })
            ));
            control_protocol::write_frame(
                &mut attachment,
                &ControlBody::Attach {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
            control_protocol::write_frame(
                &mut attachment,
                &ControlBody::Initialize {
                    request: serde_json::json!({"protocolVersion": 1}),
                },
            )
            .await
            .unwrap();
            loop {
                match control_protocol::read_frame(&mut attachment).await.unwrap() {
                    Some(ControlBody::Initialized { .. }) => break,
                    Some(ControlBody::Notify { .. }) => {}
                    frame => panic!("initialize failed: {frame:?}"),
                }
            }
            control_protocol::write_frame(
                &mut attachment,
                &ControlBody::EstablishSession {
                    method: "session/new".into(),
                    request: serde_json::json!({"cwd": checkout, "mcpServers": []}),
                },
            )
            .await
            .unwrap();
            loop {
                match control_protocol::read_frame(&mut attachment).await.unwrap() {
                    Some(ControlBody::SessionReady { .. }) => break,
                    Some(ControlBody::Notify { .. }) => {}
                    frame => panic!("session establishment failed: {frame:?}"),
                }
            }
        })
        .await
        .expect("actual SDK establishment");
        let storage = fixture.original_storage();
        let original = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(!original.runner_journal.proves_runner_quiescent());
        let starts = std::fs::read(&counter).unwrap();
        assert_eq!(starts.iter().filter(|byte| **byte == b'\n').count(), 1);
        let original_profile = storage.original_profile_identity().unwrap();
        let produced = fixture.produced_origin();
        let record = crate::process::worker_registry::load_strict(id)
            .unwrap()
            .unwrap();
        assert_eq!(record.pid, child.0.id());
        assert_eq!(record.launch_nonce, Some(fixture.nonce));
        produced.validate_record_birth(&record).unwrap();
        produced
            .validate_baseline_at(&original, original.lifecycle_generation)
            .unwrap();
        assert!(original_profile.is_durable());
        let mut view = HomeView::new(
            Some("main".into()),
            AvailableTools::detect(),
            crate::file_watch::FileWatchService::new().unwrap(),
        )
        .unwrap();
        view.view_mode = ViewMode::Structured;
        view.selected_session = Some(id.into());
        view.delete_selected(&crate::tui::dialogs::DeleteOptions {
            delete_worktree: true,
            force_delete: false,
            delete_branch: true,
            delete_sandbox: false,
            keep_scratch: false,
        })
        .unwrap();
        assert_eq!(view.deletes_in_flight.len(), 1);
        let original_request = *view.deletes_in_flight.keys().next().unwrap();
        let control = view.deletes_in_flight[&original_request].control.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        let marker = loop {
            if let Ok(bytes) = std::fs::read(&entered) {
                if let Ok(marker) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    break marker;
                }
            }
            assert!(
                Instant::now() < deadline,
                "actual original Stop did not acknowledge retirement"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(marker["pid"], child.0.id());
        assert_eq!(marker["nonce"], fixture.nonce.to_string());
        assert_eq!(marker["mode"], 0);
        let receipt = control.receipt_for_test().expect("original bound receipt");
        assert_eq!(receipt.generation(), original.lifecycle_generation + 1);
        assert_eq!(receipt.session_id(), id);
        std::fs::write(&retire_gate, "release original graceful Stop").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut hook, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("hook accept: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "original deletion did not enter its real destroy hook"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        hook.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut announcement = String::new();
        BufReader::new(hook.try_clone().unwrap())
            .read_line(&mut announcement)
            .unwrap();
        let announcement: serde_json::Value = serde_json::from_str(&announcement).unwrap();
        assert_eq!(announcement["id"], id);
        assert_eq!(announcement["cwd"], checkout.to_str().unwrap());
        let reserved = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(reserved.lifecycle_generation, receipt.generation());
        assert_eq!(
            reserved.lifecycle_reservation.as_ref().unwrap().op,
            LifecycleOperation::Purge
        );
        assert!(
            reserved.runner_journal.proves_quiescent(),
            "hook gate must be after canonical native retirement"
        );
        receipt
            .current_projection()
            .validate_baseline_at(&reserved, receipt.generation())
            .unwrap();
        assert!(control.matches(&reserved));
        let visible_before = serde_json::to_value(view.get_instance(id).unwrap()).unwrap();
        let durable_before = serde_json::to_value(&reserved).unwrap();
        let counter_before = view.deletion_poller.request_counter_for_test();
        assert!(
            !view.apply_deletion_results(),
            "original cannot complete while its hook is held"
        );
        view.open_delete_for_selected();
        assert!(view.confirm_dialog.is_some());
        assert!(
            matches!(&view.pending_force_remove_session, Some(PendingForceRemoval::Existing { request_id, .. }) if *request_id == original_request)
        );
        view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), None);
        let dialog = screen(&mut view);
        assert!(dialog.contains("Force Remove Refused"), "{dialog}");
        assert!(view.confirm_dialog.is_none());
        assert!(view.pending_force_remove_session.is_none());
        assert_eq!(
            view.deletion_poller.request_counter_for_test(),
            counter_before
        );
        assert!(!control.force_requested());
        assert_eq!(view.deletes_in_flight.len(), 1);
        assert!(view.deletes_in_flight.contains_key(&original_request));
        assert!(!view.deletes_in_flight[&original_request].attempt.forced);
        assert_eq!(
            serde_json::to_value(view.get_instance(id).unwrap()).unwrap(),
            visible_before
        );
        let durable_after = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&durable_after).unwrap(),
            durable_before
        );
        assert_eq!(
            storage.original_profile_identity().unwrap(),
            original_profile
        );
        assert!(std::sync::Arc::ptr_eq(
            &receipt,
            &view.deletes_in_flight[&original_request]
                .control
                .receipt_for_test()
                .unwrap()
        ));
        assert!(!view.failed_deletes.contains_key(id));
        assert!(!hook_done.exists());
        assert_eq!(std::fs::read(&counter).unwrap(), starts);
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "original resource"
        );
        git(&repo, &["show-ref", "--verify", "refs/heads/original"]);
        assert!(!view.apply_deletion_results());
        assert!(child.0.try_wait().unwrap().is_some());
        assert!(!crate::process::worker::is_process_group_alive(
            child.0.id()
        ));

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(view.info_dialog.is_none());
        assert!(!screen(&mut view).contains("Force Remove Refused"));

        hook.write_all(format!("{release}\n").as_bytes()).unwrap();
        hook.shutdown(std::net::Shutdown::Write).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !view.apply_deletion_results() {
            assert!(
                Instant::now() < deadline,
                "original delete did not finish after its own hook release"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(view.deletes_in_flight.is_empty());
        assert!(
            view.get_instance(id).is_none(),
            "real Removed/AlreadyGone application"
        );
        assert!(!storage.load().unwrap().iter().any(|row| row.id == id));
        assert_eq!(
            std::fs::read_to_string(&hook_done).unwrap(),
            format!("{id}\n")
        );
        assert!(
            !checkout.exists(),
            "TooLate must not change original cleanup to keep paths"
        );
        let refs = git(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/original"],
        );
        assert!(refs.stdout.is_empty());
        assert!(!socket.exists());
        assert!(!control_socket.exists());
        assert!(
            !crate::session::runner_journal::stop_socket(id, child.0.id())
                .unwrap()
                .exists()
        );
        assert!(crate::process::worker_registry::load_strict(id)
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read(&counter).unwrap(), starts);
        assert_eq!(
            view.deletion_poller.request_counter_for_test(),
            counter_before
        );
        eprintln!("HOSTED_FORCE_PENDING_UI_ACK: actual TooLate UI rendered; one original request/receipt/lease/row and native counter retained; original hook completed once; original cleanup completed");
    }
}

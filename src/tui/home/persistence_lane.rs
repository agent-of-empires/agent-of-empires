use super::persistence_worker::*;
use super::*;
use crate::session::Status;
use std::collections::VecDeque;

pub(super) enum PersistenceIntent {
    Save(Option<Box<crate::tui::stop_poller::StopRequest>>),
    Reload(ReloadKind),
    ReloadAfter(ReloadKind, ReloadContinuation),
    Transaction(TransactionEnvelope),
    Rewire(WatchTargets),
    Close,
}

struct ActivePersistence {
    id: u64,
    view_epoch: u64,
    revisions: HashMap<String, u64>,
    stop: Option<crate::tui::stop_poller::StopRequest>,
    reload_kind: Option<ReloadKind>,
    continuation: Option<ReloadContinuation>,
    transaction_rows: Vec<TransactionRowGuard>,
    transaction_failure: Option<persistence_transactions::FailureContext>,
}

pub(super) struct PersistenceLane {
    worker: crate::tui::worker::Worker<PersistenceRequest, PersistenceDone>,
    queue: VecDeque<PersistenceIntent>,
    active: Option<ActivePersistence>,
    revisions: HashMap<String, u64>,
    row_edits: HashMap<String, u64>,
    group_edits: HashMap<String, u64>,
    acknowledged: HashMap<String, u64>,
    next_id: u64,
    clock: u64,
    view_epoch: u64,
    pub closing: bool,
    pub closed: bool,
    pub failed: bool,
    pub mouse_capture: Option<bool>,
    quit_error: Option<String>,
    pub(super) created: VecDeque<CreatedContinuation>,
    pub(super) actions: VecDeque<PersistenceAction>,
    #[cfg(test)]
    pub(super) acknowledgements: VecDeque<Result<(), String>>,
}

struct TransactionRowGuard {
    id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    storage: std::sync::Arc<Storage>,
    generation: u64,
    revision: u64,
}
pub(super) struct TransactionEnvelope {
    request: Box<persistence_transactions::TransactionRequest>,
    save: SaveSnapshot,
    context: ReloadContext,
    rows: Vec<TransactionRowGuard>,
    view_epoch: u64,
    accepted_revisions: HashMap<String, u64>,
}
pub(super) enum ReloadContinuation {
    Restart {
        result: crate::session::restart::RestartResult,
        attach_after: bool,
    },
    Recovery(RecoveryUpdate),
    AttachProject {
        id: String,
        message: String,
        origin: RequestOrigin,
    },
    StoreMove {
        id: String,
        title: String,
        origin: RequestOrigin,
        resume: Option<crate::tui::app::Action>,
        container_up: bool,
    },
    AttachReturn {
        id: String,
        origin: RequestOrigin,
        updates: Vec<StatusUpdate>,
        agent: bool,
    },
}
impl ReloadContinuation {
    fn acknowledges_launch_projection(&self, snapshot: &ReloadSnapshot) -> bool {
        let (before, after) = match self {
            Self::Restart { result, .. } => (result.before.as_ref(), result.instance.as_ref()),
            Self::Recovery(update) => (update.before.as_ref(), update.instance.as_ref()),
            _ => return true,
        };
        before.id == after.id
            && before.created_at == after.created_at
            && before.same_storage_origin(after)
            && snapshot.profiles.iter().any(|profile| {
                before
                    .storage_origin
                    .as_ref()
                    .is_some_and(|original| profile.storage.same_origin_as(original))
                    && profile.rows.iter().any(|row| {
                        row.id == before.id
                            && row.created_at == before.created_at
                            && row.lifecycle_generation == after.lifecycle_generation
                            && row.active_execution == after.active_execution
                            && row.tool == before.tool
                    })
            })
    }
}

impl PersistenceLane {
    pub(super) fn new(
        worker: crate::tui::worker::Worker<PersistenceRequest, PersistenceDone>,
    ) -> Self {
        Self {
            worker,
            queue: VecDeque::new(),
            active: None,
            revisions: HashMap::new(),
            row_edits: HashMap::new(),
            group_edits: HashMap::new(),
            acknowledged: HashMap::new(),
            next_id: 0,
            clock: 0,
            view_epoch: 0,
            closing: false,
            closed: false,
            failed: false,
            mouse_capture: None,
            quit_error: None,
            created: VecDeque::new(),
            actions: VecDeque::new(),
            #[cfg(test)]
            acknowledgements: VecDeque::new(),
        }
    }

    fn edit(&mut self, profile: &str, id: Option<&str>) -> u64 {
        self.clock = self.clock.checked_add(1).expect("TUI edit clock exhausted");
        self.revisions.insert(profile.to_owned(), self.clock);
        if let Some(id) = id {
            self.row_edits.insert(id.to_owned(), self.clock);
        }
        self.clock
    }
}

fn retire_tokens(pending: &mut EditTokens, captured: &EditTokens) {
    pending.retain(|key, token| captured.get(key) != Some(token));
}

impl HomeView {
    #[cfg(test)]
    pub(super) fn persistence_is_idle(&self) -> bool {
        self.persistence.active.is_none() && self.persistence.queue.is_empty()
    }
    pub(in crate::tui) fn take_persistence_quit_error(&mut self) -> Option<String> {
        self.persistence.quit_error.take()
    }
    pub(super) fn cancel_persistence_quit(&mut self, message: String) {
        self.persistence.quit_error = Some(message);
        self.persistence.closing = false;
        self.persistence
            .queue
            .retain(|intent| !matches!(intent, PersistenceIntent::Close));
    }
    pub(in crate::tui) fn take_cancelled_persistence_quit_error(&mut self) -> Option<String> {
        if !self.persistence.closing && !self.persistence.failed && !self.persistence.closed {
            self.persistence.quit_error.take()
        } else {
            None
        }
    }
    pub(in crate::tui) fn persistence_is_closed(&self) -> bool {
        self.persistence.closed
    }

    pub(in crate::tui) fn persistence_has_failed(&self) -> bool {
        self.persistence.failed
    }

    pub(in crate::tui) fn persistence_is_closing(&self) -> bool {
        self.persistence.closing
    }

    pub(super) fn reconcile_reload_is_pending(&self) -> bool {
        self.persistence
            .active
            .as_ref()
            .is_some_and(|active| active.reload_kind == Some(ReloadKind::Reconciled))
            || self
                .persistence
                .queue
                .iter()
                .any(|intent| matches!(intent, PersistenceIntent::Reload(ReloadKind::Reconciled)))
    }

    pub(in crate::tui) fn take_persisted_mouse_capture(&mut self) -> Option<bool> {
        self.persistence.mouse_capture.take()
    }
    pub(super) fn record_row_edit(&mut self, profile: &str, id: &str) -> u64 {
        self.persistence.edit(profile, Some(id))
    }

    pub(super) fn record_group_edit(&mut self, profile: &str) -> u64 {
        let token = self.persistence.edit(profile, None);
        self.persistence
            .group_edits
            .insert(profile.to_owned(), token);
        token
    }

    pub(in crate::tui) fn request_save(&mut self) {
        if !matches!(
            self.persistence.queue.back(),
            Some(PersistenceIntent::Save(None))
        ) {
            let before_close = self
                .persistence
                .queue
                .iter()
                .position(|intent| matches!(intent, PersistenceIntent::Close))
                .unwrap_or(self.persistence.queue.len());
            self.persistence
                .queue
                .insert(before_close, PersistenceIntent::Save(None));
        }
        self.dispatch_persistence();
    }

    pub(in crate::tui) fn request_saved_stop(
        &mut self,
        request: crate::tui::stop_poller::StopRequest,
    ) {
        self.persistence
            .queue
            .push_back(PersistenceIntent::Save(Some(Box::new(request))));
        self.dispatch_persistence();
    }

    pub(in crate::tui) fn request_reload(&mut self, kind: ReloadKind) {
        if !matches!(self.persistence.queue.back(), Some(PersistenceIntent::Reload(queued)) if *queued == kind)
        {
            self.persistence
                .queue
                .push_back(PersistenceIntent::Reload(kind));
        }
        self.dispatch_persistence();
    }

    pub(super) fn request_watch_rewire(&mut self, targets: WatchTargets) {
        self.persistence
            .queue
            .push_back(PersistenceIntent::Rewire(targets));
        self.dispatch_persistence();
    }

    pub(in crate::tui) fn request_persistence_close(&mut self) {
        if self.persistence.closing {
            return;
        }

        self.persistence.quit_error = None;
        self.persistence.closing = true;
        self.request_save();
        self.persistence.queue.push_back(PersistenceIntent::Close);
        self.dispatch_persistence();
    }
    pub(super) fn capture_save_snapshot(&self) -> SaveSnapshot {
        let profiles = self
            .storages
            .iter()
            .map(|(name, storage)| {
                let rows: Vec<_> = self
                    .instances
                    .values()
                    .filter(|row| {
                        row.source_profile == *name
                            && self.creating_stub_id.as_deref() != Some(&row.id)
                    })
                    .cloned()
                    .collect();
                let mut additions = self.pending_added.get(name).cloned().unwrap_or_default();
                additions.retain(|id, _| {
                    rows.iter()
                        .any(|row| row.id == *id && row.status != Status::Deleting)
                });
                ProfileSaveSnapshot {
                    name: name.clone(),
                    storage: storage.clone(),
                    revision: self.persistence.revisions.get(name).copied().unwrap_or(0),
                    rows,
                    groups: self
                        .group_trees
                        .get(name)
                        .map(GroupTree::get_all_groups)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|g| {
                            self.persistence.group_edits.contains_key(name)
                                && (self.creating_provisional_profile.as_deref()
                                    != Some(name.as_str())
                                    || !self.creating_provisional_group_paths.contains(&g.path))
                        })
                        .collect(),
                    deletions: self
                        .pending_deletions
                        .get(name)
                        .cloned()
                        .unwrap_or_default(),
                    group_deletions: self
                        .pending_group_deletions
                        .get(name)
                        .cloned()
                        .unwrap_or_default(),
                    additions,
                }
            })
            .collect();
        SaveSnapshot { profiles }
    }

    pub(super) fn request_transaction(
        &mut self,
        request: persistence_transactions::TransactionRequest,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let save = self.capture_save_snapshot();
        self.enqueue_transaction(request, save)
    }
    pub(super) fn enqueue_transaction(
        &mut self,
        request: persistence_transactions::TransactionRequest,
        mut save: SaveSnapshot,
    ) -> anyhow::Result<super::TransactionDisposition> {
        if self.persistence.closing || self.persistence.closed || self.persistence.failed {
            let context = request.failure_context();
            self.reject_transaction(Some(&context), "Persistence is not accepting transactions");
            anyhow::bail!("Persistence is not accepting transactions");
        }
        if matches!(
            &request,
            persistence_transactions::TransactionRequest::SwitchProfile { .. }
                | persistence_transactions::TransactionRequest::CreateProfile(_)
                | persistence_transactions::TransactionRequest::DeleteProfile(_)
        ) {
            self.persistence.view_epoch = self
                .persistence
                .view_epoch
                .checked_add(1)
                .expect("view epoch exhausted");
        }
        request.try_for_each_captured_storage(|captured| {
            if !save
                .profiles
                .iter()
                .any(|p| p.storage.same_origin_as(captured))
            {
                save.profiles.push(ProfileSaveSnapshot {
                    name: captured.profile().to_owned(),
                    storage: captured.clone(),
                    revision: self
                        .persistence
                        .revisions
                        .get(captured.profile())
                        .copied()
                        .unwrap_or(0),
                    rows: Vec::new(),
                    groups: Vec::new(),
                    deletions: RowDeletions::new(),
                    group_deletions: EditTokens::new(),
                    additions: EditTokens::new(),
                });
            }
            Ok(())
        })?;
        let mut rows = Vec::new();
        request.try_for_each_captured_row(|row| {
            rows.push(TransactionRowGuard {
                id: row.before.id.clone(),
                created_at: row.before.created_at,
                storage: row.origin.storage.clone(),
                generation: row.origin.generation,
                revision: self
                    .persistence
                    .row_edits
                    .get(&row.before.id)
                    .copied()
                    .unwrap_or(0),
            });
            Ok(())
        })?;
        let mut storages = self.storages.clone();
        request.try_for_each_captured_storage(|storage| {
            if !storages
                .get(storage.profile())
                .is_some_and(|known| known.same_origin_as(storage))
            {
                storages.insert(storage.profile().to_owned(), storage.clone());
            }
            Ok(())
        })?;
        let envelope = TransactionEnvelope {
            request: Box::new(request),
            save,
            context: ReloadContext {
                storages,
                active_profile: self.active_profile.clone(),
                kind: ReloadKind::Full,
            },
            rows,
            view_epoch: self.persistence.view_epoch,
            accepted_revisions: self.persistence.revisions.clone(),
        };
        self.persistence
            .queue
            .push_back(PersistenceIntent::Transaction(envelope));
        self.dispatch_persistence();
        Ok(super::TransactionDisposition::Queued)
    }
    pub(super) fn request_reload_after(
        &mut self,
        kind: ReloadKind,
        continuation: ReloadContinuation,
    ) {
        if self.persistence.closing || self.persistence.closed || self.persistence.failed {
            self.info_dialog = Some(InfoDialog::new(
                "Persistence unavailable",
                "The requested continuation was not acknowledged.",
            ));
            return;
        }
        self.persistence
            .queue
            .push_back(PersistenceIntent::ReloadAfter(kind, continuation));
        self.dispatch_persistence();
    }
    pub(in crate::tui) fn request_attach_return_reload(
        &mut self,
        id: String,
        origin: RequestOrigin,
        updates: Vec<StatusUpdate>,
        agent: bool,
    ) {
        self.request_reload_after(
            ReloadKind::Full,
            ReloadContinuation::AttachReturn {
                id,
                origin,
                updates,
                agent,
            },
        );
    }
    pub(in crate::tui) fn take_persistence_action(&mut self) -> Option<PersistenceAction> {
        self.persistence.actions.pop_front()
    }
    pub(super) fn transaction_is_pending(&self, id: &str) -> bool {
        self.persistence
            .active
            .as_ref()
            .is_some_and(|a| a.transaction_rows.iter().any(|r| r.id == id))
            || self.persistence.queue.iter().any(
                |i| matches!(i,PersistenceIntent::Transaction(e) if e.rows.iter().any(|r|r.id==id)),
            )
    }
    fn transaction_rows_match(&self, rows: &[TransactionRowGuard]) -> bool {
        rows.iter().all(|guard| {
            self.instances.get(&guard.id).is_some_and(|current| {
                current.created_at == guard.created_at
                    && current.lifecycle_generation == guard.generation
                    && current
                        .storage_origin
                        .as_ref()
                        .is_some_and(|s| guard.storage.same_origin_as(s))
                    && self
                        .persistence
                        .row_edits
                        .get(&guard.id)
                        .copied()
                        .unwrap_or(0)
                        == guard.revision
            })
        })
    }

    fn dispatch_persistence(&mut self) {
        if self.persistence.active.is_some() || self.persistence.closed || self.persistence.failed {
            return;
        }
        let Some(mut intent) = self.persistence.queue.pop_front() else {
            return;
        };
        if matches!(intent, PersistenceIntent::Close)
            && self.persistence.quit_error.is_none()
            && self.storages.keys().any(|profile| {
                self.persistence
                    .revisions
                    .get(profile)
                    .copied()
                    .unwrap_or(0)
                    > self
                        .persistence
                        .acknowledged
                        .get(profile)
                        .copied()
                        .unwrap_or(0)
            })
        {
            self.persistence.queue.push_front(PersistenceIntent::Close);
            intent = PersistenceIntent::Save(None);
        }
        self.persistence.next_id = self
            .persistence
            .next_id
            .checked_add(1)
            .expect("TUI request clock exhausted");
        let id = self.persistence.next_id;
        let mut stop = None;
        let mut reload_kind = None;
        let mut continuation = None;
        let mut transaction_rows = Vec::new();
        let mut transaction_failure = None;
        let mut active_epoch = self.persistence.view_epoch;
        let job = match intent {
            PersistenceIntent::Save(after) => {
                stop = after.map(|request| *request);
                PersistenceJob::Save(self.capture_save_snapshot())
            }
            PersistenceIntent::Reload(kind) => {
                reload_kind = Some(kind);
                PersistenceJob::Reload(ReloadContext {
                    storages: self.storages.clone(),
                    active_profile: self.active_profile.clone(),
                    kind,
                })
            }
            PersistenceIntent::ReloadAfter(kind, after) => {
                reload_kind = Some(kind);
                continuation = Some(after);
                PersistenceJob::Reload(ReloadContext {
                    storages: self.storages.clone(),
                    active_profile: self.active_profile.clone(),
                    kind,
                })
            }
            PersistenceIntent::Transaction(envelope) => {
                if !self.transaction_rows_match(&envelope.rows)
                    || envelope.save.profiles.iter().any(|p| {
                        self.storages
                            .get(&p.name)
                            .is_some_and(|s| !s.same_origin_as(&p.storage))
                            || self
                                .persistence
                                .revisions
                                .get(&p.name)
                                .copied()
                                .unwrap_or(0)
                                != envelope
                                    .accepted_revisions
                                    .get(&p.name)
                                    .copied()
                                    .unwrap_or(0)
                    })
                    || (matches!(
                        envelope.request.as_ref(),
                        persistence_transactions::TransactionRequest::SwitchProfile { .. }
                            | persistence_transactions::TransactionRequest::CreateProfile(_)
                            | persistence_transactions::TransactionRequest::DeleteProfile(_)
                    ) && envelope.view_epoch != self.persistence.view_epoch)
                {
                    #[cfg(test)]
                    self.persistence.acknowledgements.push_back(Err(
                        "Original transaction changed before dispatch".to_owned(),
                    ));
                    let context = envelope.request.failure_context();
                    self.reject_transaction(Some(&context),"The original row, profile or relevant edit changed before its save fence was acknowledged. No continuation was submitted.");
                    self.dispatch_persistence();
                    return;
                }
                active_epoch = envelope.view_epoch;
                transaction_failure = Some(envelope.request.failure_context());
                transaction_rows = envelope.rows;
                PersistenceJob::Transaction {
                    save: envelope.save,
                    context: envelope.context,
                    request: envelope.request,
                }
            }
            PersistenceIntent::Rewire(targets) => PersistenceJob::Rewire(targets),
            PersistenceIntent::Close => PersistenceJob::Close,
        };
        self.persistence.active = Some(ActivePersistence {
            id,
            view_epoch: active_epoch,
            revisions: self.persistence.revisions.clone(),
            stop,
            reload_kind,
            continuation,
            transaction_rows,
            transaction_failure,
        });
        if self
            .persistence
            .worker
            .try_request(PersistenceRequest { id, job })
            .is_err()
        {
            if let Some(active) = self.persistence.active.take() {
                self.reject_transaction(
                    active.transaction_failure.as_ref(),
                    "Persistence worker rejected the request before acknowledgement",
                );
            }
            self.persistence.failed = true;
            self.persistence.quit_error = Some("Persistence worker rejected the request; pending edits and native continuations were not acknowledged".to_owned());
            self.info_dialog = Some(InfoDialog::new("Persistence unavailable", "The persistence worker rejected the request. Pending edits and native continuations remain unacknowledged."));
        }
    }

    fn apply_watch_pass(&mut self, pass: WatchPass) {
        self.disk_watch.installed = pass.disk;
        self.config_watch.installed = pass.config;
        self.reload_failure_state
            .apply_disk_watcher_init_pass(pass.disk_error);
        self.reload_failure_state
            .apply_config_watcher_init_pass(pass.config_error);
    }

    pub(in crate::tui) fn apply_persistence_results(&mut self) -> bool {
        let done = match self.persistence.worker.try_recv() {
            Ok(done) => done,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if !self.persistence.closed && !self.persistence.failed {
                    self.persistence.failed = true;
                    self.persistence.quit_error = Some(
                        "Persistence worker stopped before save and watcher-close acknowledgements"
                            .to_owned(),
                    );
                    self.info_dialog = Some(InfoDialog::new("Persistence unavailable", "The persistence worker stopped. Pending edits and native continuations were not acknowledged."));
                    return true;
                }
                return false;
            }
        };
        let Some(active) = self.persistence.active.take() else {
            return false;
        };
        if active.id != done.id {
            self.persistence.active = Some(active);
            return false;
        }
        #[cfg(test)]
        {
            let result = match &done.result {
                PersistenceResult::Save(profiles) => profiles
                    .iter()
                    .find_map(|p| p.result.as_ref().err().map(|e| format!("{e:#}")))
                    .map_or(Ok(()), Err),
                PersistenceResult::Reload(result) => {
                    result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))
                }
                PersistenceResult::Transaction { result, .. } => {
                    result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))
                }
                _ => Ok(()),
            };
            self.persistence.acknowledgements.push_back(result);
        }
        match done.result {
            PersistenceResult::Save(profiles) => self.apply_save_ack(profiles, &active),
            PersistenceResult::Reload(result) => match result {
                Ok(snapshot) if active.view_epoch == self.persistence.view_epoch => {
                    let native_projection_acknowledged = active
                        .continuation
                        .as_ref()
                        .is_none_or(|after| after.acknowledges_launch_projection(&snapshot));
                    self.apply_reload_snapshot(snapshot, &active.revisions);
                    self.reload_failure_state.record_storage(&Ok(()));
                    if let Some(after) = active.continuation {
                        if native_projection_acknowledged {
                            self.apply_reload_continuation(after);
                        } else {
                            self.reject_reload_continuation(after, "The original native producer result does not match the worker's acknowledged physical row");
                        }
                    }
                    if active.reload_kind == Some(ReloadKind::Reconciled) {
                        self.pending_reconcile_reload = false;
                        self.reconcile_reload_retry_at = None;
                        self.release_startup_recovery_gate(true);
                    }
                }
                Ok(_) => {
                    if let Some(after) = active.continuation {
                        self.reject_reload_continuation(
                            after,
                            "The view changed before reload acknowledgement",
                        );
                    }
                }
                Err(error) => {
                    if active.reload_kind == Some(ReloadKind::Reconciled) {
                        self.reconcile_reload_retry_at =
                            Some(std::time::Instant::now() + Self::RECONCILE_RELOAD_RETRY_INTERVAL);
                    }
                    let message = format!("{error:#}");
                    self.reload_failure_state.record_storage(&Err(error));
                    if let Some(after) = active.continuation {
                        self.reject_reload_continuation(after, &message);
                    }
                }
            },
            PersistenceResult::Transaction {
                saved,
                result,
                failure_snapshot,
            } => {
                let rows_match = self.transaction_rows_match(&active.transaction_rows);
                self.apply_save_ack(saved, &active);
                match result {
                    Ok(done) => {
                        let done = *done;
                        if !rows_match {
                            self.reject_transaction(active.transaction_failure.as_ref(),"A newer relevant edit or lifecycle replaced the accepted view. The durable transaction committed, but its stale native/UI continuation was not submitted.");
                        } else if active.view_epoch == self.persistence.view_epoch {
                            if let Some(context) = active.transaction_failure.as_ref() {
                                for (before, revision) in &context.metadata {
                                    let ack = self
                                        .persistence
                                        .acknowledged
                                        .entry(before.source_profile.clone())
                                        .or_default();
                                    *ack = (*ack).max(*revision);
                                }
                            }
                            if let persistence_transactions::TransactionEffect::Switched {
                                profile,
                            } = &done.effect
                            {
                                self.active_profile = profile.clone();
                                self.clear_profile_projection_state();
                            }
                            if let persistence_transactions::TransactionEffect::ProfileDeleted(
                                name,
                            ) = &done.effect
                            {
                                if self.active_profile.as_deref() == Some(name) {
                                    self.active_profile = None;
                                }
                                self.clear_profile_projection_state();
                            }
                            if let Some(snapshot) = done.snapshot {
                                self.apply_reload_snapshot(snapshot, &active.revisions);
                            }
                            self.apply_transaction_effect(done.effect);
                        } else {
                            self.apply_transaction_native_effect(done.effect);
                        }
                    }
                    Err(error) => {
                        self.reject_transaction(
                            active.transaction_failure.as_ref(),
                            &format!("{error:#}. No native/UI continuation was acknowledged."),
                        );
                        if active.view_epoch == self.persistence.view_epoch {
                            if let Some(snapshot) = failure_snapshot {
                                self.apply_reload_snapshot(snapshot, &active.revisions);
                            }
                        }
                    }
                }
            }
            PersistenceResult::Rewired(pass) => self.apply_watch_pass(pass),
            PersistenceResult::Closed => self.persistence.closed = true,
        }
        self.dispatch_persistence();
        true
    }

    fn apply_save_ack(&mut self, profiles: Vec<ProfileSaveDone>, active: &ActivePersistence) {
        let mut stop_saved = false;
        for done in profiles {
            let snapshot = done.snapshot;
            let same_profile = self
                .storages
                .get(&snapshot.name)
                .is_some_and(|storage| storage.same_origin_as(&snapshot.storage));
            if !same_profile {
                continue;
            }
            match done.result {
                Ok(peer_deleted) => {
                    let stop_absent = active
                        .stop
                        .as_ref()
                        .is_some_and(|request| peer_deleted.contains(&request.session_id));
                    if let Some(pending) = self.pending_deletions.get_mut(&snapshot.name) {
                        pending.retain(|id, deletion| snapshot.deletions.get(id) != Some(deletion));
                    }
                    if let Some(pending) = self.pending_group_deletions.get_mut(&snapshot.name) {
                        retire_tokens(pending, &snapshot.group_deletions);
                    }
                    if let Some(pending) = self.pending_added.get_mut(&snapshot.name) {
                        retire_tokens(pending, &snapshot.additions);
                    }
                    self.persistence
                        .acknowledged
                        .insert(snapshot.name.clone(), snapshot.revision);
                    let absent: Vec<_> = peer_deleted
                        .into_iter()
                        .filter(|id| {
                            self.instances.get(id).is_some_and(|row| {
                                row.source_profile == snapshot.name
                                    && snapshot.rows.iter().any(|captured| {
                                        captured.id == *id && captured.created_at == row.created_at
                                    })
                                    && row.storage_origin.as_ref().is_some_and(|origin| {
                                        snapshot.storage.same_origin_as(origin)
                                    })
                            })
                        })
                        .collect();
                    self.drop_peer_deleted_rows(&absent);
                    stop_saved |= active.stop.as_ref().is_some_and(|request| {
                        request.instance.source_profile == snapshot.name
                            && snapshot.rows.iter().any(|captured| {
                                captured.id == request.session_id
                                    && captured.created_at == request.instance.created_at
                            })
                            && request
                                .instance
                                .storage_origin
                                .as_ref()
                                .is_some_and(|origin| snapshot.storage.same_origin_as(origin))
                            && !stop_absent
                    });
                }
                Err(error) => {
                    if self.persistence.closing {
                        self.persistence.quit_error = Some(format!(
                            "Profile {} was not saved: {error:#}",
                            snapshot.name
                        ));
                        self.persistence.closing = false;
                        self.persistence
                            .queue
                            .retain(|intent| !matches!(intent, PersistenceIntent::Close));
                    }
                    self.info_dialog = Some(InfoDialog::new(
                        "Save failed",
                        &format!(
                            "Profile '{}': {error:#}. Pending edits are retained.",
                            snapshot.name
                        ),
                    ));
                }
            }
        }
        if stop_saved {
            if let Some(request) = active.stop.as_ref() {
                self.stop_poller
                    .request_stop(crate::tui::stop_poller::StopRequest {
                        session_id: request.session_id.clone(),
                        instance: request.instance.clone(),
                    });
            }
        }
    }

    fn reject_transaction(
        &mut self,
        context: Option<&persistence_transactions::FailureContext>,
        message: &str,
    ) {
        if self.persistence.closing {
            self.cancel_persistence_quit(message.to_owned());
        }
        if let Some(context) = context {
            if let Some(id) = &context.creation {
                if self.creating_stub_id.as_deref() == Some(id) {
                    self.creating_stub_id = None;
                    self.creation_cancel = None;
                    if self.instances.get(id).is_some_and(|r| {
                        r.status == Status::Creating
                            && r.lifecycle_generation == 0
                            && r.lifecycle_reservation.is_none()
                    }) {
                        self.remove_creation_stub(id);
                    }
                    self.creating_hook_progress.remove(id);
                    self.creating_provisional_group_paths.clear();
                    self.creating_provisional_profile = None;
                    self.rebuild_group_trees();
                    self.rebuild_flat_items_keeping_cursor();
                }
            }
            if context.store_move {
                self.store_move_in_flight = None;
            }
            for (before, revision) in &context.metadata {
                if self.persistence.row_edits.get(&before.id) == Some(revision)
                    && self.instances.get(&before.id).is_some_and(|r| {
                        r.created_at == before.created_at
                            && r.same_storage_origin(before)
                            && r.lifecycle_generation == before.lifecycle_generation
                    })
                {
                    let mut restored = before.clone();
                    if let Some(current) = self.instances.get(&before.id) {
                        restored.merge_runtime_from_reload(current);
                    }
                    self.instances.insert(before.id.clone(), restored);
                    self.record_row_edit(&before.source_profile, &before.id);
                }
            }
        }
        self.info_dialog = Some(InfoDialog::new(
            context.map_or("Transaction failed", |c| c.title),
            message,
        ));
    }
    fn clear_profile_projection_state(&mut self) {
        self.selected_session = None;
        self.selected_group = None;
        self.selected_group_profile = None;
        self.preview_cache = PreviewCache::default();
        self.terminal_preview_cache = PreviewCache::default();
        self.container_terminal_preview_cache = PreviewCache::default();
        self.tool_preview_cache = PreviewCache::default();
        self.preview_scroll_offset = 0;
        self.search_active = false;
        self.search_query = Input::default();
        self.search_matches.clear();
        self.search_match_index = 0;
    }
    fn apply_transaction_native_effect(
        &mut self,
        effect: persistence_transactions::TransactionEffect,
    ) {
        use persistence_transactions::TransactionEffect;
        match effect {
            TransactionEffect::PresentationSaved => {}
            TransactionEffect::QuitConfirmationDisabled => {
                self.confirm_before_quit = false;
            }
            TransactionEffect::Restart {
                request,
                origin,
                attach,
            } => {
                let id = request.session_id.clone();
                if self
                    .instances
                    .get(&id)
                    .is_some_and(|row| !origin.matches(row))
                {
                    self.info_dialog=Some(InfoDialog::new("Restart Failed","The original row advanced before launch acknowledgement. No native request was submitted."));
                    return;
                }
                if self.restart_poller.request_restart(*request).is_ok() {
                    self.restart_in_flight.insert(id.clone(), origin);
                    if attach {
                        self.attach_after_restart.insert(id.clone());
                    }
                    if let Some(row) = self.instances.get_mut(&id) {
                        row.status = Status::Starting;
                        row.last_error = None;
                        row.last_start_time = Some(std::time::Instant::now());
                    }
                } else {
                    self.info_dialog=Some(InfoDialog::new("Restart Failed","The native restart worker rejected the saved request. The row was not marked Starting."));
                }
            }
            TransactionEffect::Settlement { request, origin } => {
                self.submit_runner_settlement(origin, *request)
            }
            TransactionEffect::AttachProject { request, origin } => {
                let id = request.original.session_id().to_owned();
                if self.attach_project_poller.request_attach(request).is_ok() {
                    self.attach_project_in_flight.insert(id, origin);
                } else {
                    self.info_dialog = Some(InfoDialog::new(
                        "Could Not Attach Project",
                        "The native attach worker rejected the saved request.",
                    ));
                }
            }
            TransactionEffect::StoreMove(request) => {
                if self.store_move_poller.request_move(*request).is_err() {
                    self.store_move_in_flight = None;
                    self.info_dialog = Some(InfoDialog::new(
                        "Agent Store Move Failed",
                        "The native copy worker rejected the saved request.",
                    ));
                }
            }
            TransactionEffect::AdmissionCancelledBeforeEffect(id) => {
                if self.creating_stub_id.as_deref() == Some(id.as_str()) {
                    self.creating_stub_id = None;
                    self.creation_cancel = None;
                    self.remove_creation_stub(&id);
                    self.creating_provisional_group_paths.clear();
                    self.creating_provisional_profile = None;
                }
            }
            TransactionEffect::Admitted(request) => {
                let id = request.admitted_instance.id.clone();
                if let Some(stub) = self.instances.get_mut(&id) {
                    stub.storage_origin = Some(request.storage.clone());
                }
                if let Err(error) = self.creation_poller.request_creation(*request) {
                    if self.creating_stub_id.as_deref() == Some(id.as_str()) {
                        self.creating_stub_id = None;
                        self.creation_cancel = None;
                        self.remove_creation_stub(&id);
                        self.creating_provisional_group_paths.clear();
                        self.creating_provisional_profile = None;
                    }
                    self.info_dialog = Some(InfoDialog::new(
                        "Creation Failed",
                        &format!("Original admission retained: {error:#}"),
                    ));
                }
            }
            TransactionEffect::Trash(request) => {
                self.project_transaction_rows(vec![request.instance.clone()]);
                self.trash_poller.request_trash(*request);
            }
            TransactionEffect::DeleteGroup {
                requests,
                profiles,
                path,
            } => {
                for profile in profiles {
                    if let Some(tree) = self.group_trees.get_mut(&profile) {
                        tree.delete_group(&path);
                    }
                }
                for request in requests {
                    if let Some(row) = self.instances.get_mut(&request.session_id) {
                        if row.created_at == request.instance.created_at
                            && row.same_storage_origin(&request.instance)
                        {
                            row.status = Status::Deleting;
                            row.group_path.clear();
                        }
                    }
                    self.request_deletion(request);
                }
                self.rebuild_flat_items_keeping_cursor();
            }
            TransactionEffect::CreationWithdrawn { original, ack } => {
                self.finish_creation_withdrawal(original, ack)
            }
            TransactionEffect::Created { row, .. } => {
                if self.creating_stub_id.as_deref() == Some(&row.id) {
                    self.creating_stub_id = None;
                    self.creation_cancel = None;
                    self.remove_creation_stub(&row.id);
                    self.creating_hook_progress.remove(&row.id);
                    self.creating_provisional_group_paths.clear();
                    self.creating_provisional_profile = None;
                }
                self.info_dialog=Some(InfoDialog::new("Session created in original profile",&format!("{} was durably published in {}. The changed view was not retargeted or auto-attached.",row.title,row.source_profile)));
            }
            _ => {}
        }
    }
    fn apply_transaction_effect(&mut self, effect: persistence_transactions::TransactionEffect) {
        use persistence_transactions::TransactionEffect;
        match effect {
            TransactionEffect::Ordered {
                rows,
                id,
                destination,
                warning,
                stale,
            } => {
                self.project_transaction_rows(rows);
                self.rebuild_flat_items_keeping_cursor();
                if stale {
                    self.flash_status("Rows changed elsewhere, refreshing the list");
                } else if let (Some(id), Some(target)) = (id, destination) {
                    self.after_committed_cross_group_move(
                        &id,
                        &target,
                        warning.map_or(Ok(()), |message| Err(anyhow::anyhow!(message))),
                    );
                }
            }
            TransactionEffect::Edited { rows, warning } => {
                self.project_transaction_rows(rows);
                if let Some(warning) = warning {
                    self.info_dialog = Some(InfoDialog::new("Rename Saved with Warning", &warning));
                }
            }
            TransactionEffect::MetadataCommitted { rows, after } => {
                self.project_transaction_rows(rows);
                use persistence_transactions::MetadataContinuation;
                match after {
                    MetadataContinuation::None => {}
                    MetadataContinuation::Reseat(id) => self.select_session_by_id(&id),
                    MetadataContinuation::Snooze { id, message } => {
                        if self.sort_order == SortOrder::Attention {
                            self.select_top_attention(None);
                        } else {
                            self.select_session_by_id(&id);
                        }
                        self.persistence
                            .actions
                            .push_back(PersistenceAction::Status(message));
                    }
                    MetadataContinuation::Unread(id) => {
                        if self.get_instance(&id).is_some_and(|r| r.is_unread()) {
                            self.manual_unread_hold = Some(id.clone());
                        } else if self.manual_unread_hold.as_deref() == Some(&id) {
                            self.manual_unread_hold = None;
                        }
                        self.select_session_by_id(&id);
                    }
                }
            }
            TransactionEffect::Switched { .. } => {
                self.refresh_from_config(ConfigRefreshOrigin::Interactive)
            }
            TransactionEffect::ProfileDeleted(_) => self.show_profile_picker(),
            TransactionEffect::ClaimAbortConfirmation(selection) => {
                let message = format!("Remove intent {} from {}? All resources, native history and path exclusions are retained permanently. This does not stop a process or perform Undo/Purge; the ID remains reserved.", selection.id(), selection.origin().profile());
                self.pending_creation_confirmation = None;
                self.pending_claim_abort_confirmation = Some(selection);
                self.confirm_dialog = Some(
                    ConfirmDialog::new("Abort intent metadata", &message, "creation_recovery")
                        .buttons("Abort metadata", "Cancel"),
                );
            }
            TransactionEffect::ClaimAborted(ack) => {
                self.info_dialog = Some(InfoDialog::new("Intent metadata aborted", &format!("{} removed from {}; resources and permanent exclusions retained. No native cleanup performed.", ack.id(), ack.origin().profile())));
            }
            TransactionEffect::CreationConfirmation(confirmation) => {
                let id = &confirmation.capture.id;
                let (title,message,button)=match confirmation.action {
                    persistence_transactions::CreationRecoveryAction::RetryPublication=>("Retry original creation publication",format!("Publish retained original {id} in {} using its same original creation acknowledgement? Native creation is not rerun.",confirmation.capture.storage.profile()),"Publish"),
                    persistence_transactions::CreationRecoveryAction::Undo=>("Undo original creation",format!("Undo retained original {id} in {} only if actual native/resource proof covers every original effect? Missing, changed, dirty or preexisting resources are retained; this is not force Purge.",confirmation.capture.storage.profile()),"Undo"),
                };
                self.pending_claim_abort_confirmation = None;
                self.pending_creation_confirmation = Some(confirmation);
                self.confirm_dialog = Some(
                    ConfirmDialog::new(title, &message, "creation_recovery")
                        .buttons(button, "Cancel"),
                );
            }
            TransactionEffect::CreationWithdrawn { original, ack } => {
                self.finish_creation_withdrawal(original, ack);
            }
            TransactionEffect::CreationAlreadyPublished(id) => {
                self.info_dialog=Some(InfoDialog::new("Original creation already published",&format!("{id} is already published; native creation and its create counter were not repeated.")));
            }
            TransactionEffect::Created {
                row,
                hooks_ran,
                warnings,
                auto_attach,
            } => {
                let id = row.id.clone();
                if self.creating_stub_id.as_deref() == Some(id.as_str()) {
                    self.creating_stub_id = None;
                    self.creation_cancel = None;
                    self.creating_provisional_group_paths.clear();
                    self.creating_provisional_profile = None;
                    self.creating_hook_progress.remove(&id);
                    self.new_dialog = None;
                }
                let origin = match RequestOrigin::capture(&row) {
                    Ok(origin) => origin,
                    Err(error) => {
                        self.info_dialog=Some(InfoDialog::new("Original publication acknowledged without attach",&format!("The original creation committed, but its original namespace changed before handoff: {error:#}. A replacement row was not projected or attached.")));
                        return;
                    }
                };
                self.publish_persisted_instance(*row);
                self.rebuild_group_trees();
                if hooks_ran {
                    self.on_launch_hooks_ran.insert(id.clone(), origin.clone());
                }
                if auto_attach {
                    self.select_and_reveal_session(&id);
                    self.persistence
                        .created
                        .push_back(CreatedContinuation { id, origin });
                } else {
                    self.info_dialog=Some(InfoDialog::new("Creation finished before cancellation","The original creation was durably published before cancellation took effect. It was not automatically attached or claimed safely withdrawn."));
                }
                if !warnings.is_empty() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Session warnings",
                        &warnings.join("\n\n"),
                    ));
                }
            }
            TransactionEffect::Archived {
                row,
                reveal,
                successor,
            } => {
                self.project_transaction_rows(vec![*row]);
                if reveal {
                    self.reveal_archived_section();
                    self.rebuild_flat_items();
                }
                if self.sort_order == SortOrder::Attention {
                    self.select_top_attention(None);
                } else if let Some(next) = successor {
                    self.select_session_by_id(&next);
                } else {
                    self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
                    self.selected_session = None;
                    self.selected_group = None;
                    self.selected_group_profile = None;
                }
            }
            TransactionEffect::Restored { id, outcome } => {
                use super::operations::RestoreFromTrash;
                match outcome {
                    RestoreFromTrash::Restored => {
                        self.rebuild_flat_items();
                        self.select_session_by_id(&id);
                    }
                    RestoreFromTrash::AlreadyGone => self.drop_peer_deleted_rows(&[id]),
                    RestoreFromTrash::Busy(reason)
                    | RestoreFromTrash::WorktreeFailed { reason } => {
                        self.info_dialog = Some(InfoDialog::new("Restore Failed", &reason))
                    }
                    RestoreFromTrash::PersistFailed => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Restore Failed",
                            "The original restore was not persisted.",
                        ))
                    }
                }
            }
            native => self.apply_transaction_native_effect(native),
        }
    }
    fn finish_creation_withdrawal(
        &mut self,
        original: persistence_transactions::CreationRecoveryCapture,
        _ack: crate::session::builder::CreationWithdrawalAck,
    ) {
        // The opaque worker-matched acknowledgement is produced only by actual original Undo.
        let id = original.id;
        if self.instances.get(&id).is_some_and(|row| {
            row.created_at == original.created_at
                && row.lifecycle_generation == original.generation
                && row
                    .storage_origin
                    .as_ref()
                    .is_some_and(|s| s.same_origin_as(&original.storage))
        }) {
            self.instances.shift_remove(&id);
        }
        if self.creating_stub_id.as_deref() == Some(id.as_str()) {
            self.creating_stub_id = None;
            self.creation_cancel = None;
            self.creating_provisional_group_paths.clear();
            self.creating_provisional_profile = None;
            self.creating_hook_progress.remove(&id);
        }
        self.rebuild_group_trees();
        self.rebuild_flat_items_keeping_cursor();
        self.info_dialog=Some(InfoDialog::new("Original creation undone",&format!("{id}: the actual same-Create native/resource withdrawal was acknowledged. No force Purge or replacement generation was used.")));
    }
    fn reject_reload_continuation(&mut self, after: ReloadContinuation, message: &str) {
        if let ReloadContinuation::AttachProject { id, .. } = after {
            self.attach_project_in_flight.remove(&id);
        }
        self.info_dialog = Some(InfoDialog::new(
            "Reload Failed",
            &format!("{message}. The dependent action was not resumed."),
        ));
    }
    fn apply_reload_continuation(&mut self, after: ReloadContinuation) {
        match after {
            ReloadContinuation::Restart {
                result,
                attach_after,
            } => self.apply_completed_restart(result, attach_after),
            ReloadContinuation::Recovery(update) => self.apply_completed_recovery(update),
            ReloadContinuation::AttachProject {
                id,
                message,
                origin,
            } => {
                self.attach_project_in_flight.remove(&id);
                if self.get_instance(&id).is_some_and(|row| {
                    row.created_at == origin.created_at
                        && row.lifecycle_generation >= origin.generation
                        && row
                            .storage_origin
                            .as_ref()
                            .is_some_and(|s| origin.storage.same_origin_as(s))
                }) {
                    self.info_dialog = Some(InfoDialog::new("Project Attached", &message));
                } else {
                    self.info_dialog=Some(InfoDialog::new("Project Attach Result Stale","The original session disappeared or was replaced; its success was not projected onto another row."));
                }
            }
            ReloadContinuation::StoreMove {
                id,
                title,
                origin,
                resume,
                container_up,
            } => {
                let same = self
                    .get_instance(&id)
                    .is_some_and(|row| origin.matches(row));
                if same && (container_up || !self.sandbox_store_move_pending(&id)) {
                    if let Some(action) = resume {
                        if container_up {
                            self.store_move_bypass = Some(id.clone());
                        }
                        self.persistence
                            .actions
                            .push_back(PersistenceAction::Resume { id, origin, action });
                    }
                } else {
                    self.info_dialog=Some(InfoDialog::new("Agent Store Still Shared",&format!("The original agent store of '{title}' was not acknowledged as moved; the launch was not resumed.")));
                }
            }
            ReloadContinuation::AttachReturn {
                id,
                origin,
                updates,
                agent,
            } => {
                if !self
                    .get_instance(&id)
                    .is_some_and(|row| origin.matches(row))
                {
                    self.info_dialog=Some(InfoDialog::new("Original attach result changed","The original attached row changed or was replaced; status, unread and selection were not applied to another row."));
                    return;
                }
                self.apply_status_updates_without_hooks(updates);
                if agent {
                    self.clear_unread_on_view(&id);
                    self.stamp_last_accessed(&id);
                    self.request_save();
                }
                if self.sort_order == SortOrder::Attention {
                    self.select_top_attention(Some(&id));
                } else {
                    self.select_session_by_id(&id);
                }
            }
        }
    }

    fn apply_reload_snapshot(&mut self, snapshot: ReloadSnapshot, captured: &HashMap<String, u64>) {
        let mut rows = Vec::new();
        let mut storages = HashMap::new();
        let mut trees = HashMap::new();
        for load in snapshot.profiles {
            let same_profile = self
                .storages
                .get(&load.name)
                .is_some_and(|storage| storage.same_origin_as(&load.storage));
            let dirty = same_profile
                && self
                    .persistence
                    .revisions
                    .get(&load.name)
                    .copied()
                    .unwrap_or(0)
                    > self
                        .persistence
                        .acknowledged
                        .get(&load.name)
                        .copied()
                        .unwrap_or(0);
            let newer = same_profile
                && self
                    .persistence
                    .revisions
                    .get(&load.name)
                    .copied()
                    .unwrap_or(0)
                    > captured.get(&load.name).copied().unwrap_or(0);
            let mut loaded = load.rows;
            if same_profile {
                for row in &mut loaded {
                    if let Some(current) = self.instances.get(&row.id).filter(|current| {
                        current.created_at == row.created_at
                            && current
                                .storage_origin
                                .as_ref()
                                .is_some_and(|origin| load.storage.same_origin_as(origin))
                    }) {
                        if current.lifecycle_generation > row.lifecycle_generation {
                            *row = current.clone();
                        } else if (dirty || newer)
                            && self.persistence.row_edits.contains_key(&row.id)
                        {
                            row.tool.clone_from(&current.tool);
                            row.command.clone_from(&current.command);
                            row.extra_args.clone_from(&current.extra_args);
                        }
                        row.merge_runtime_from_reload(current);
                    }
                }
                if dirty || newer {
                    for current in self
                        .instances
                        .values()
                        .filter(|row| row.source_profile == load.name)
                    {
                        if self
                            .pending_added
                            .get(&load.name)
                            .is_some_and(|pending| pending.contains_key(&current.id))
                            && self.creating_stub_id.as_deref() != Some(&current.id)
                            && !loaded.iter().any(|row| row.id == current.id)
                        {
                            loaded.push(current.clone());
                        }
                    }
                }
            } else {
                self.pending_deletions.remove(&load.name);
                self.pending_group_deletions.remove(&load.name);
                self.pending_added.remove(&load.name);
            }
            let mut groups = load.groups;
            let dirty_groups = same_profile
                && self
                    .persistence
                    .group_edits
                    .get(&load.name)
                    .copied()
                    .unwrap_or(0)
                    > self
                        .persistence
                        .acknowledged
                        .get(&load.name)
                        .copied()
                        .unwrap_or(0);
            if dirty_groups {
                if let Some(deleted) = self.pending_group_deletions.get(&load.name) {
                    groups.retain(|group| !deleted.contains_key(&group.path));
                }
                if let Some(tree) = self.group_trees.get(&load.name) {
                    for current in tree.get_all_groups() {
                        if let Some(group) =
                            groups.iter_mut().find(|group| group.path == current.path)
                        {
                            *group = current;
                        } else {
                            groups.push(current);
                        }
                    }
                }
            }
            trees.insert(
                load.name.clone(),
                GroupTree::new_with_groups(&loaded, &groups),
            );
            rows.extend(loaded);
            storages.insert(load.name, load.storage);
        }
        if let Some(stub) = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id))
        {
            if storages.get(&stub.source_profile).is_some_and(|storage| {
                stub.storage_origin
                    .as_ref()
                    .is_some_and(|origin| storage.same_origin_as(origin))
            }) {
                rows.push(stub.clone());
            }
        }
        self.storages = storages;
        self.group_trees = trees;
        self.instances = Self::build_instances_map(rows);
        self.legacy_duplicate_reports = snapshot.reports;
        self.registered_projects = snapshot.projects;
        if let Some(configs) = snapshot.hook_configs {
            self.status_hook_configs = configs;
            if let Some(status_hooks) = self.status_hook_configs.get(&snapshot.config_profile) {
                self.status_hook_config = status_hooks.clone();
            }
        }
        self.persistence.mouse_capture = snapshot.mouse_capture;
        self.apply_watch_pass(snapshot.watches);
        self.remote_owner_cache.borrow_mut().clear();
        self.rebuild_flat_items_keeping_cursor();
        let preserve_live_selection = self.live_send.as_ref().is_some_and(|state| {
            self.selected_session.as_deref() == Some(state.session_id.as_str())
        });
        if self.search_active && !self.search_query.value().is_empty() && !preserve_live_selection {
            self.update_search();
        } else if self.search_active || !self.search_matches.is_empty() {
            self.refresh_search_matches();
        }
        if !preserve_live_selection {
            self.update_selected();
        }
        if let Some(state) = self.live_send.clone() {
            self.end_live_send_on_drift(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn acknowledged_equal_generation_publishes_metadata_without_losing_runtime() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("lane").unwrap();
        let mut seed = Instance::new("before", "/tmp/before");
        seed.lifecycle_generation = 1;
        let id = seed.id.clone();
        storage
            .update(|rows, _| {
                rows.push(seed);
                Ok(())
            })
            .unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        view.instances.get_mut(&id).unwrap().last_error = Some("live runtime".into());
        let generation = view.instances[&id].lifecycle_generation;
        storage
            .update(|rows, _| {
                rows[0].title = "acknowledged".into();
                rows[0].group_path = "committed-group".into();
                rows[0].archive();
                Ok(())
            })
            .unwrap();
        let acknowledged = storage.load().unwrap().remove(0);
        assert_eq!(acknowledged.lifecycle_generation, generation);
        view.project_transaction_rows(vec![acknowledged]);
        let current = &view.instances[&id];
        assert_eq!(current.title, "acknowledged");
        assert_eq!(current.group_path, "committed-group");
        assert!(current.is_archived());
        assert_eq!(current.last_error.as_deref(), Some("live runtime"));
        let before = serde_json::to_value(current).unwrap();
        let mut obsolete = current.clone();
        obsolete.lifecycle_generation = generation.checked_sub(1).unwrap();
        obsolete.title = "obsolete plan".into();
        obsolete.last_error = Some("obsolete runtime".into());
        view.project_transaction_rows(vec![obsolete]);
        assert_eq!(serde_json::to_value(&view.instances[&id]).unwrap(), before);
        assert_eq!(
            view.instances[&id].last_error.as_deref(),
            Some("live runtime")
        );
    }

    #[test]
    #[serial_test::serial]
    fn presentation_and_quit_preferences_are_ordered_behind_real_flocks() {
        let _home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("lane").unwrap();
        update_config(|config| config.session.confirm_before_quit = true).unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        view.persistence.acknowledgements.clear();
        let app_dir = crate::session::get_app_dir().unwrap();
        let state_lock =
            crate::session::acquire_storage_flock(&app_dir, ".state.toml.lock").unwrap();
        let config_lock = crate::session::acquire_storage_flock(
            &app_dir,
            crate::session::config::CONFIG_LOCK_FILENAME,
        )
        .unwrap();
        view.apply_sort_order(SortOrder::Attention);
        view.apply_sort_order(SortOrder::Newest);
        view.apply_group_by(GroupByMode::Project);
        view.disable_confirm_before_quit();
        view.request_persistence_close();
        view.apply_persistence_results();
        assert_eq!(view.sort_order, SortOrder::Newest);
        assert_eq!(view.group_by, GroupByMode::Project);
        assert!(view.confirm_before_quit());
        assert!(view.persistence.acknowledgements.is_empty());
        assert!(!view.persistence_is_closed());
        drop(state_lock);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while view.persistence.acknowledgements.len() < 3 {
            view.apply_persistence_results();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let state = crate::session::config::AppStateConfig::load().unwrap();
        assert_eq!(state.sort_order, Some(SortOrder::Newest));
        assert_eq!(state.group_by, Some(GroupByMode::Project));
        assert!(view.persistence.acknowledgements.iter().all(Result::is_ok));
        assert!(view.confirm_before_quit());
        assert!(!view.persistence_is_closed());
        drop(config_lock);
        drain(&mut view);
        assert!(!view.confirm_before_quit());
        assert!(
            !crate::session::Config::load()
                .unwrap()
                .session
                .confirm_before_quit
        );
        assert!(view.persistence_is_closed());
        assert!(view.persistence.acknowledgements.iter().all(Result::is_ok));
    }

    #[test]
    #[serial_test::serial]
    fn failed_quit_preference_ack_cancels_close_and_keeps_input_admission() {
        let _home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("lane").unwrap();
        update_config(|config| config.session.confirm_before_quit = true).unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        let lock_path = crate::session::get_app_dir()
            .unwrap()
            .join(crate::session::config::CONFIG_LOCK_FILENAME);
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        view.disable_confirm_before_quit();
        view.request_persistence_close();
        drain(&mut view);
        assert!(view.confirm_before_quit());
        assert!(!view.persistence_is_closing());
        assert!(!view.persistence_is_closed());
        assert!(view.take_cancelled_persistence_quit_error().is_some());
        assert!(view.info_dialog.is_some());
        std::fs::remove_dir(lock_path).unwrap();
        view.apply_sort_order(SortOrder::Attention);
        drain(&mut view);
        assert_eq!(
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .sort_order,
            Some(SortOrder::Attention)
        );
        view.request_persistence_close();
        drain(&mut view);
        assert!(view.persistence_is_closed());
    }

    fn drain(view: &mut HomeView) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !view.persistence_is_idle() {
            view.apply_persistence_results();
            assert!(
                std::time::Instant::now() < deadline,
                "persistence acknowledgement did not arrive"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    #[serial_test::serial]
    fn save_then_reload_keeps_the_newer_expanded_group_while_workspace_is_held() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("lane").unwrap();
        let mut tree = GroupTree::new_with_groups(&[], &[]);
        tree.create_group("alpha");
        storage
            .update(|_, groups| {
                *groups = tree.get_all_groups();
                Ok(())
            })
            .unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        view.group_trees
            .get_mut("lane")
            .unwrap()
            .set_collapsed("alpha", true);
        view.record_group_edit("lane");
        let workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        view.request_save();
        view.group_trees
            .get_mut("lane")
            .unwrap()
            .set_collapsed("alpha", false);
        view.record_group_edit("lane");
        view.request_save();
        view.request_reload(ReloadKind::Storage);
        assert!(
            !view.group_trees["lane"]
                .get_all_groups()
                .iter()
                .find(|group| group.path == "alpha")
                .unwrap()
                .collapsed
        );
        drop(workspace);
        drain(&mut view);
        let (_, groups) = storage.load_with_groups().unwrap();
        assert!(
            !groups
                .iter()
                .find(|group| group.path == "alpha")
                .unwrap()
                .collapsed
        );
        assert!(
            !view.group_trees["lane"]
                .get_all_groups()
                .iter()
                .find(|group| group.path == "alpha")
                .unwrap()
                .collapsed
        );
        view.request_persistence_close();
        drain(&mut view);
        assert!(view.persistence_is_closed());
    }

    #[test]
    #[serial_test::serial]
    fn stale_save_cannot_write_or_evict_a_same_id_replacement() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("lane").unwrap();
        let original = Instance::new("original", "/tmp/metadata-only-original");
        let id = original.id.clone();
        storage
            .update(|rows, _| {
                rows.push(original);
                Ok(())
            })
            .unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        let workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        view.mutate_instance(&id, |row| row.command = "old edit".into());
        let deletion_revision = view.record_row_edit("lane", &id);
        view.pending_deletions
            .entry("lane".into())
            .or_default()
            .insert(
                id.clone(),
                RowDeletion {
                    revision: deletion_revision,
                    created_at: view.instances[&id].created_at,
                },
            );
        view.request_save();
        let mut replacement = Instance::new("replacement", "/tmp/metadata-only-replacement");
        replacement.id.clone_from(&id);
        replacement.command = "replacement command".into();
        assert_ne!(replacement.created_at, view.instances[&id].created_at);
        {
            let _identity = crate::session::acquire_session_identity_lock().unwrap();
            storage
                .update_under_workspace_claim_lock(|rows, _| {
                    rows.clear();
                    rows.push(replacement);
                    Ok(())
                })
                .unwrap();
        }
        let replacement = storage.load().unwrap().remove(0);
        view.instances.insert(id.clone(), replacement);
        drop(workspace);
        drain(&mut view);
        assert_eq!(view.instances[&id].title, "replacement");
        assert_eq!(storage.load().unwrap()[0].command, "replacement command");
        view.request_reload(ReloadKind::Storage);
        drain(&mut view);
        assert_eq!(view.instances[&id].command, "replacement command");
        view.request_persistence_close();
        drain(&mut view);
    }

    #[test]
    #[serial_test::serial]
    fn failed_quit_save_keeps_edits_and_does_not_close_the_worker() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("lane").unwrap();
        let mut row = Instance::new("original", "/tmp/metadata-only-quit");
        row.source_profile = "lane".into();
        let id = row.id.clone();
        storage
            .update(|rows, _| {
                rows.push(row);
                Ok(())
            })
            .unwrap();
        let mut view = HomeView::new_for_test(
            Some("lane".into()),
            AvailableTools::with_tools(&[]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        drain(&mut view);
        view.mutate_instance(&id, |row| row.extra_args = "--pending-edit".into());
        let profile = crate::session::get_profile_dir_path("lane").unwrap();
        let retained = profile.with_file_name("retained-original");
        std::fs::rename(&profile, &retained).unwrap();
        view.request_persistence_close();
        drain(&mut view);
        assert!(!view.persistence_is_closed());
        assert!(!view.persistence_is_closing());
        assert_eq!(view.instances[&id].extra_args, "--pending-edit");
        assert!(
            view.persistence.revisions["lane"]
                > view
                    .persistence
                    .acknowledged
                    .get("lane")
                    .copied()
                    .unwrap_or(0)
        );
        assert!(view.persistence.quit_error.is_some());
        std::fs::rename(retained, profile).unwrap();
        view.request_persistence_close();
        drain(&mut view);
        assert!(view.persistence_is_closed());
        assert_eq!(storage.load().unwrap()[0].extra_args, "--pending-edit");
    }
}

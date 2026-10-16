//! Creating a session: the request, its pending stub, and the cleanup a
//! cancel or a quit has to do.

use super::*;

pub(in crate::tui) struct SessionMutationGuards {
    pub(super) _session_title: crate::session::StorageFlock,
    pub(super) _lifecycle: crate::session::StorageFlock,
}

pub(super) struct CancelledCreation {
    pub(super) profile: String,
    pub(super) provisional: std::sync::Arc<crate::session::builder::ProvisionalCreation>,
    groups: HashSet<String>,
    finished: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ProvisionalRetirement {
    Settled,
    Deferred,
}

fn retire_provisional_stub(
    profile: &str,
    id: &str,
    provisional: &crate::session::builder::ProvisionalCreation,
    provisional_groups: &HashSet<String>,
) -> anyhow::Result<ProvisionalRetirement> {
    let ownership = crate::session::storage::try_acquire_ownership_lock()?.ok_or_else(|| {
        anyhow::anyhow!(
            "Session operation in progress; cancellation cleanup will retry after the worker stops"
        )
    })?;
    let _identity =
        crate::session::storage::try_acquire_session_identity_lock_with_ownership(&ownership)?
            .ok_or_else(|| anyhow::anyhow!("Session operation in progress"))?;
    let _title =
        crate::session::storage::try_acquire_session_title_lock_with_ownership(&ownership, id)?
            .ok_or_else(|| anyhow::anyhow!("Session operation in progress"))?;
    let storage = Storage::open_unwatched_with_ownership(profile, &ownership)?;
    let _lifecycle = storage
        .try_acquire_instance_lifecycle_lock_with_ownership(&ownership, id)?
        .ok_or_else(|| anyhow::anyhow!("Session operation in progress"))?;
    provisional.verify_profile(&storage)?;
    storage
        .try_update_with_ownership(&ownership, |rows, groups| {
            if let Some(index) = rows.iter().position(|row| row.id == id) {
                if !provisional.matches_instance(&rows[index])? {
                    return Ok(ProvisionalRetirement::Settled);
                }
                if rows[index]
                    .lifecycle_reservation
                    .as_ref()
                    .is_some_and(|lease| lease.resource_intent.is_some())
                {
                    return Ok(ProvisionalRetirement::Deferred);
                }
                rows.remove(index);
            }
            groups.retain(|group| !provisional_groups.contains(&group.path));
            *groups = GroupTree::new_with_groups(rows, groups).get_all_groups();
            Ok(ProvisionalRetirement::Settled)
        })?
        .ok_or_else(|| anyhow::anyhow!("Session operation in progress"))
}

impl HomeView {
    fn forget_creation_stub(
        &mut self,
        id: &str,
        provisional: Option<&crate::session::builder::ProvisionalCreation>,
    ) {
        if provisional.is_none() {
            self.remove_instance(id);
            return;
        }
        if let Some(instance) = self.instances.shift_remove(id) {
            if let Some(deletions) = self.pending_deletions.get_mut(&instance.source_profile) {
                deletions.remove(id);
            }
            if let Some(added) = self.pending_added.get_mut(&instance.source_profile) {
                added.remove(id);
            }
        }
    }

    /// Request background session creation, used for sandbox sessions so the UI does not
    /// block. A `Status::Creating` stub appears in the list, so progress shows in the
    /// preview pane while the TUI stays usable.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    ) {
        // Pre-resolve the title with the logic the builder will run, so the stub, the
        // background creation and the final instance agree; otherwise an empty title shows
        // as the path basename in the stub and a civilization name in the instance.
        if data.title.is_empty() {
            let existing_titles: Vec<&str> = self
                .instances()
                .filter(|i| i.source_profile == data.profile)
                .map(|i| i.title.as_str())
                .collect();
            let existing_branches: Vec<&str> = self
                .instances()
                .filter(|i| i.source_profile == data.profile)
                .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
                .collect();
            let taken_branches = crate::session::builder::collect_taken_branches_for_derived_dedupe(
                &existing_branches,
                &data.path,
                &data.extra_repo_paths,
                data.worktree_enabled,
                data.create_new_branch,
                data.scratch,
            );
            if let Ok(title) = crate::session::builder::resolve_title(
                &data.title,
                data.worktree_branch.as_deref(),
                data.worktree_enabled,
                &existing_titles,
                &taken_branches,
            ) {
                data.title = title;
            }
        }
        let stub_title = data.title.clone();
        let mut stub = Instance::new(&stub_title, &data.path);
        stub.tool = if data.tool.is_empty() {
            "claude".to_string()
        } else {
            data.tool.clone()
        };
        stub.group_path = data.group.clone();
        stub.status = crate::session::Status::Creating;
        stub.yolo_mode = data.yolo_mode;
        stub.source_profile = data.profile.clone();
        if let Err(error) = stub.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        ) {
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Creation Failed",
                &error.to_string(),
            ));
            self.new_dialog = None;
            return;
        }

        // Set stub worktree_info so project-mode grouping works during creation; the real
        // one, with a resolved main_repo_path, replaces it once build_instance completes.
        let stub_branch = data
            .worktree_branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                data.worktree_enabled
                    .then(|| crate::session::builder::branch_name_from_title(&stub_title))
            });
        if let Some(branch) = stub_branch {
            stub.worktree_info = Some(crate::session::WorktreeInfo {
                branch,
                main_repo_path: data.path.clone(),
                managed_by_aoe: false,
                created_at: chrono::Utc::now(),
                base_branch: data.base_branch.clone(),
            });
        }

        let provisional_creation = match (|| -> anyhow::Result<_> {
            let Some(root) = crate::session::storage::try_acquire_ownership_lock()? else {
                return Ok(None);
            };
            let storage =
                Storage::open_with_ownership(&data.profile, self.file_watch.clone(), &root)?;
            let Some(_identity) =
                crate::session::storage::try_acquire_session_identity_lock_with_ownership(&root)?
            else {
                return Ok(None);
            };
            let Some(_lifecycle) =
                storage.try_acquire_instance_lifecycle_lock_with_ownership(&root, &stub.id)?
            else {
                return Ok(None);
            };
            let saved = storage.try_update_with_ownership(&root, |rows, _| {
                anyhow::ensure!(
                    !rows.iter().any(|row| row.id == stub.id),
                    "Creating identity already exists"
                );
                let provisional =
                    crate::session::builder::ProvisionalCreation::capture(&storage, &stub)?;
                rows.push(stub.clone());
                Ok(provisional)
            })?;
            // This insertion never changes groups. Its only write publishes atomically,
            // so a returned write error needs no compensating Root acquisition.
            Ok(saved.map(std::sync::Arc::new))
        })() {
            Ok(Some(provisional)) => Some(provisional),
            Ok(None) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Session operation in progress",
                    "Another session operation is in progress. No changes were saved. Try again when it finishes.",
                ));
                return;
            }
            Err(error) => {
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Creation Failed",
                    &format!("{error:#}"),
                ));
                self.new_dialog = None;
                return;
            }
        };
        let stub_id = stub.id.clone();
        let target_profile = data.profile.clone();
        let existing_group_paths: HashSet<String> = self
            .group_trees
            .get(&target_profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .map(|group| group.path)
                    .collect()
            })
            .unwrap_or_default();

        // Add stub to instance list
        self.publish_persisted_instance(stub);
        self.rebuild_group_trees();
        if !data.group.is_empty() {
            if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                tree.create_group(&data.group);
            }
        }
        self.creating_provisional_group_paths = self
            .group_trees
            .get(&target_profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .map(|group| group.path)
                    .filter(|path| !existing_group_paths.contains(path))
                    .collect()
            })
            .unwrap_or_default();

        // Initialize progress tracking and select the stub
        self.creating_hook_progress.insert(
            stub_id.clone(),
            CreatingHookProgress {
                hook_output: Vec::new(),
                current_hook: None,
            },
        );
        self.creating_stub_id = Some(stub_id.clone());
        self.creating_provisional_creation = provisional_creation.clone();
        self.rebuild_flat_items();

        // Move cursor to the new stub
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &stub_id))
        {
            self.cursor = pos;
            self.update_selected();
        }

        // Close the dialog
        self.new_dialog = None;

        let cancel = tokio_util::sync::CancellationToken::new();
        self.creation_cancel = Some(cancel.clone());
        // Filter out the stub from existing instances so the builder doesn't
        // treat its placeholder title as a duplicate to auto-increment.
        let existing_instances: Vec<Instance> = self
            .instances
            .values()
            .filter(|i| i.id != stub_id)
            .cloned()
            .collect();
        let request = CreationRequest {
            data,
            existing_instances,
            hooks,
            cancel,
            provisional_creation,
        };
        if let Err(error) = self.creation_poller.request_creation(request) {
            self.cancel_creation();
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Creation Failed",
                &format!("{error:#}"),
            ));
        }
    }

    /// Hide the cancelled creation while its worker finishes the hook and verified rollback.
    pub fn cancel_creation(&mut self) {
        if let Some(cancel) = self.creation_cancel.take() {
            cancel.cancel();
        }
        if let Some(stub_id) = self.creating_stub_id.take() {
            let provisional = self.creating_provisional_creation.take();
            let groups = std::mem::take(&mut self.creating_provisional_group_paths);
            let profile = self
                .get_instance(&stub_id)
                .map(|row| row.source_profile.clone());
            self.forget_creation_stub(&stub_id, provisional.as_deref());
            if let (Some(provisional), Some(profile)) = (provisional, profile) {
                self.cancelled_creations.insert(
                    stub_id.clone(),
                    CancelledCreation {
                        profile,
                        provisional,
                        groups,
                        finished: false,
                    },
                );
                let cancelled = self.cancelled_creations.get(&stub_id).unwrap();
                if let Err(error) = retire_provisional_stub(
                    &cancelled.profile,
                    &stub_id,
                    &cancelled.provisional,
                    &cancelled.groups,
                ) {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Cancellation cleanup failed",
                        &format!("{error:#}"),
                    ));
                } else if let Err(error) = self.reload() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Cancellation reload failed",
                        &format!("{error:#}"),
                    ));
                }
            }
            self.creating_hook_progress.remove(&stub_id);
            self.rebuild_group_trees();
            self.rebuild_flat_items();
            self.update_selected();
        }
        self.new_dialog = None;
    }

    fn retire_finished_cancelled_creations(&mut self) {
        if self.cancelled_creations.is_empty() {
            return;
        }
        let ids: Vec<_> = self
            .cancelled_creations
            .iter()
            .filter(|(_, cancelled)| cancelled.finished)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let cancelled = self.cancelled_creations.get(&id).unwrap();
            match retire_provisional_stub(
                &cancelled.profile,
                &id,
                &cancelled.provisional,
                &cancelled.groups,
            ) {
                Ok(ProvisionalRetirement::Settled) => {
                    self.cancelled_creations.remove(&id);
                    if let Err(error) = self.reload() {
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Cancellation reload failed",
                            &format!("{error:#}"),
                        ));
                    }
                }
                Ok(ProvisionalRetirement::Deferred) => {}
                Err(error) => {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Cancellation cleanup pending",
                        &format!("{error:#}"),
                    ));
                }
            }
        }
    }

    /// Apply any pending creation results from the background poller.
    /// Returns Some(session_id) if creation succeeded and we should attach.
    pub fn apply_creation_results(&mut self) -> Option<String> {
        use crate::tui::creation_poller::CreationResult;
        self.retire_finished_cancelled_creations();

        let outcome = match self.creation_poller.try_recv_result() {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if self.creating_stub_id.is_some() || !self.cancelled_creations.is_empty() {
                    self.cancel_creation();
                    self.cancelled_creations.clear();
                    let recovery = Storage::recover_pending_moves(self.file_watch.clone());
                    let message = match recovery {
                        Ok(notices) => {
                            format!("Creation worker disconnected.\n{}", notices.join("\n"))
                        }
                        Err(error) => {
                            format!("Creation worker disconnected; recovery failed: {error:#}")
                        }
                    };
                    if let Err(error) = self.reload() {
                        tracing::warn!(target: "tui.home", "Creation disconnect reload failed: {error:#}");
                    }
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &message));
                }
                return None;
            }
        };
        let result = outcome.result;

        // A late result must settle its own cancelled placeholder, never a newer request's.
        if outcome.cancelled || matches!(result, CreationResult::Cancelled) {
            match result {
                CreationResult::Success(mut success) => {
                    if let Err(error) = success.build_result.rollback() {
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Cancellation cleanup failed",
                            &format!("{error:#}"),
                        ));
                    }
                }
                CreationResult::Error(error) => {
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                }
                CreationResult::Cancelled => {}
            }
            if let Some(provisional) = outcome.provisional_creation {
                let id = provisional.instance_id();
                if self.cancelled_creations.get(id).is_some_and(|cancelled| {
                    std::sync::Arc::ptr_eq(&cancelled.provisional, &provisional)
                }) {
                    self.cancelled_creations.get_mut(id).unwrap().finished = true;
                    self.retire_finished_cancelled_creations();
                }
            }
            return None;
        }

        self.creation_cancel = None;
        let stub_id = self.creating_stub_id.take();
        let provisional = self.creating_provisional_creation.take();
        // Taken, not borrowed, so every early return leaves the field empty: the
        // provisional group paths belong to this stub alone and must not carry into the
        // next creation.
        let provisional_group_paths = std::mem::take(&mut self.creating_provisional_group_paths);
        if let Some(ref id) = stub_id {
            self.creating_hook_progress.remove(id);
        }

        match result {
            CreationResult::Success(success) => {
                let crate::tui::creation_poller::CreationSuccess {
                    mut build_result,
                    on_launch_hooks_ran,
                } = *success;
                if let Err(mut error) = build_result.publish() {
                    if let Err(cleanup) = build_result.rollback() {
                        error = error
                            .context(format!("Creation rollback could not finish: {cleanup:#}"));
                    }
                    if let (Some(id), Some(provisional)) = (stub_id, provisional) {
                        self.forget_creation_stub(&id, Some(&provisional));
                        self.cancelled_creations.insert(
                            id.clone(),
                            CancelledCreation {
                                profile: build_result.profile_storage.profile().to_owned(),
                                provisional,
                                groups: provisional_group_paths,
                                finished: true,
                            },
                        );
                        let cancelled = self.cancelled_creations.get(&id).unwrap();
                        match retire_provisional_stub(
                            &cancelled.profile,
                            &id,
                            &cancelled.provisional,
                            &cancelled.groups,
                        ) {
                            Ok(ProvisionalRetirement::Settled) => {
                                self.cancelled_creations.remove(&id);
                            }
                            Ok(ProvisionalRetirement::Deferred) => {}
                            Err(cleanup) => {
                                error = error
                                    .context(format!("Placeholder cleanup failed: {cleanup:#}"));
                            }
                        }
                    }
                    if let Err(reload) = self.reload() {
                        error = error.context(format!("Authoritative reload failed: {reload:#}"));
                    }
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Creation Failed",
                        &format!("{error:#}"),
                    ));
                    self.new_dialog = None;
                    return None;
                }
                let mut instance = build_result.instance;
                let session_id = instance.id.clone();
                let target_profile = build_result.profile_storage.profile().to_owned();
                self.storages
                    .insert(target_profile, build_result.profile_storage);
                crate::tips::record_session_creations(1);
                self.publish_persisted_instance(instance.clone());
                self.rebuild_group_trees();
                if on_launch_hooks_ran {
                    self.on_launch_hooks_ran.insert(session_id.clone());
                }
                if let Err(error) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {error:#}");
                }
                let minted = instance
                    .sandbox_info
                    .as_mut()
                    .map(|sandbox| std::mem::take(&mut sandbox.before_start_env))
                    .unwrap_or_default();
                if !minted.is_empty() {
                    self.mutate_instance(&session_id, |instance| {
                        if let Some(sandbox) = instance.sandbox_info.as_mut() {
                            sandbox.before_start_env = minted.clone();
                        }
                    });
                }
                self.select_and_reveal_session(&session_id);
                self.new_dialog = None;
                if !build_result.warnings.is_empty() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Session warnings",
                        &format!(
                            "Session was created, but setup emitted warnings:\n\n{}",
                            build_result.warnings.join("\n\n")
                        ),
                    ));
                }
                Some(session_id)
            }
            CreationResult::Error(mut error) => {
                if let Some(id) = stub_id {
                    let profile = self
                        .get_instance(&id)
                        .map(|instance| instance.source_profile.clone());
                    let had_provisional = provisional.is_some();
                    self.forget_creation_stub(&id, provisional.as_deref());
                    if let Some(profile) = profile {
                        let instances = self.cloned_instances_for_profile(&profile);
                        if let Some(tree) = self.group_trees.get_mut(&profile) {
                            let mut groups = tree.get_all_groups();
                            groups.retain(|group| !provisional_group_paths.contains(&group.path));
                            *tree = GroupTree::new_with_groups(&instances, &groups);
                        }
                        if let Some(provisional) = provisional {
                            self.cancelled_creations.insert(
                                id.clone(),
                                CancelledCreation {
                                    profile,
                                    provisional,
                                    groups: provisional_group_paths,
                                    finished: true,
                                },
                            );
                            let cancelled = self.cancelled_creations.get(&id).unwrap();
                            match retire_provisional_stub(
                                &cancelled.profile,
                                &id,
                                &cancelled.provisional,
                                &cancelled.groups,
                            ) {
                                Ok(ProvisionalRetirement::Settled) => {
                                    self.cancelled_creations.remove(&id);
                                }
                                Ok(ProvisionalRetirement::Deferred) => {}
                                Err(retire_error) => {
                                    error.push_str(&format!(
                                        "\nFailed to retire creation placeholder: {retire_error:#}"
                                    ));
                                }
                            }
                        }
                    }
                    if had_provisional && !self.cancelled_creations.contains_key(&id) {
                        if let Err(reload_error) = self.reload() {
                            error.push_str(&format!(
                                "\nFailed to reload authoritative state: {reload_error:#}"
                            ));
                        }
                    }
                    self.rebuild_flat_items();
                    self.update_selected();
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                } else if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_loading(false);
                    dialog.set_error(error);
                }
                None
            }
            // Returned early above.
            CreationResult::Cancelled => None,
        }
    }

    /// Check if on_launch hooks already ran for this session (and consume the flag).
    pub fn take_on_launch_hooks_ran(&mut self, session_id: &str) -> bool {
        self.on_launch_hooks_ran.remove(session_id)
    }

    /// Check if there's a pending creation operation
    pub fn is_creation_pending(&self) -> bool {
        self.creation_poller.is_pending()
    }

    /// Check if the currently selected session is the in-flight creating stub
    pub fn is_creating_stub_selected(&self) -> bool {
        match (&self.creating_stub_id, &self.selected_session) {
            (Some(stub_id), Some(selected)) => stub_id == selected,
            _ => false,
        }
    }

    /// Show a confirmation dialog warning that a session is being created.
    pub fn show_quit_during_creation_confirm(&mut self) {
        self.confirm_dialog = Some(ConfirmDialog::new(
            "Session Creating",
            "A session is still being created. Quit anyway? The hook will be cancelled.",
            "quit_during_creation",
        ));
    }

    /// Whether `q` on the home screen should confirm before quitting.
    pub fn confirm_before_quit(&self) -> bool {
        self.confirm_before_quit
    }

    /// Show the "quit aoe?" confirmation, with a "don't warn me again"
    /// checkbox that flips `confirm_before_quit` off when ticked (#1569).
    pub fn show_quit_confirm(&mut self) {
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Quit Agent of Empires",
                "Quit?\nYour sessions persist in the background.",
                "quit",
            )
            .neutral()
            .offering_dont_ask_again(),
        );
    }

    /// Persist `confirm_before_quit = false` and update the cached flag, when the user
    /// ticks "don't warn me again" in the quit dialog.
    pub(in crate::tui) fn disable_confirm_before_quit(&mut self) {
        self.confirm_before_quit = false;
        if let Err(e) = update_config(|config| {
            config.session.confirm_before_quit = false;
        }) {
            tracing::warn!(target: "tui.home", "Failed to save config: {e}");
        }
    }

    /// Persist the "don't warn me again" opt-out for whichever confirm offered the
    /// checkbox. Both call sites route through this, so keyboard and click cannot disagree
    /// about which confirms are opt-out-able; actions without a checkbox never reach it.
    pub(in crate::tui) fn apply_confirm_dont_ask_again(&mut self, action: &str) {
        match action {
            "quit" => self.disable_confirm_before_quit(),
            // Written globally, matching the quit opt-out. A profile that overrides
            // confirm_delete = true keeps prompting; that override is cleared from the
            // settings pane.
            "trash_session" => {
                if let Err(e) = update_config(|config| {
                    config.session.confirm_delete = false;
                }) {
                    tracing::warn!(target: "tui.home", "Failed to save config: {e}");
                }
            }
            _ => {}
        }
    }

    /// Cancel pending creation and drain rollback results until the shutdown deadline.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.creation_poller.is_pending() {
            return;
        }
        self.cancel_creation();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.creation_poller.is_pending() {
            let Some(outcome) = self
                .creation_poller
                .recv_result_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                break;
            };
            if let crate::tui::creation_poller::CreationResult::Success(mut success) =
                outcome.result
            {
                if let Err(error) = success.build_result.rollback() {
                    tracing::error!(target: "tui.home", "Cancelled creation exit cleanup failed: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;

    #[test]
    #[serial_test::serial]
    fn finished_cancelled_creation_stays_hidden_until_its_intent_settles() {
        use crate::session::move_journal::{
            IntentOwner, ProfileAuthority, ResourceEffect, ResourceIntent,
        };
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("owner").unwrap();
        let mut row = Instance::new("creating", "/creating");
        row.source_profile = "owner".into();
        row.status = Status::Creating;
        row.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )
        .unwrap();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let provisional = std::sync::Arc::new(
            crate::session::builder::ProvisionalCreation::capture(&storage, &row).unwrap(),
        );
        let admitted = {
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            let _identity =
                crate::session::storage::acquire_session_identity_lock_with_ownership(&root)
                    .unwrap();
            let _lifecycle = storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &row.id)
                .unwrap();
            let profile = ProfileAuthority::capture(&storage).unwrap();
            let intent = ResourceIntent::new(
                IntentOwner {
                    profile: profile.clone(),
                    id: row.id.clone(),
                    created_at: row.created_at,
                    generation: row.lifecycle_generation,
                },
                profile,
                crate::session::LifecycleOperation::Create,
                Some(row.clone()),
                Some(row.clone()),
                vec![ResourceEffect::Callback {
                    name: "creation-callbacks".into(),
                    directory: row.project_path.clone().into(),
                    checkouts: Vec::new(),
                }],
                Vec::new(),
            )
            .unwrap();
            crate::session::claim::ResourceReservation::admit_with_ownership(
                &storage,
                &root,
                intent,
                row.clone(),
            )
            .unwrap()
        };
        let mut view = HomeView::new_for_test(
            Some("owner".into()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        view.forget_creation_stub(&row.id, Some(&provisional));
        view.cancelled_creations.insert(
            row.id.clone(),
            CancelledCreation {
                profile: "owner".into(),
                provisional,
                groups: HashSet::new(),
                finished: true,
            },
        );
        view.retire_finished_cancelled_creations();
        assert!(view.cancelled_creations.contains_key(&row.id));
        view.reload().unwrap();
        assert!(view.get_instance(&row.id).is_none());
        assert_eq!(
            storage.load().unwrap()[0].lifecycle_reservation,
            admitted.instance.lifecycle_reservation
        );
        admitted.transaction.rollback().unwrap();
        view.retire_finished_cancelled_creations();
        assert!(!view.cancelled_creations.contains_key(&row.id));
        assert!(storage.load().unwrap().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn provisional_retirement_returns_before_root_and_nested_locks_are_released() {
        for layer in [
            "root",
            "identity",
            "title",
            "lifecycle",
            "transition",
            "storage",
        ] {
            let _app = crate::session::test_support::isolate_app_dir();
            let storage = Storage::new_unwatched("owner").unwrap();
            let mut row = Instance::new("creating", "/creating");
            row.source_profile = "owner".into();
            row.status = Status::Creating;
            row.try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Create,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            let provisional =
                crate::session::builder::ProvisionalCreation::capture(&storage, &row).unwrap();
            let namespace = crate::session::get_app_dir().unwrap();
            let profile = storage.sessions_path().parent().unwrap().to_path_buf();
            let id = row.id.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (released_tx, released_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let held: Box<dyn std::any::Any> = match layer {
                    "root" => Box::new(crate::session::storage::acquire_ownership_lock().unwrap()),
                    "identity" => Box::new(
                        crate::session::storage::acquire_storage_flock(
                            &namespace,
                            ".title-mutation.lock",
                        )
                        .unwrap(),
                    ),
                    "title" => Box::new(
                        crate::session::storage::acquire_storage_flock(
                            &namespace,
                            &format!(".session-title-{id}.lock"),
                        )
                        .unwrap(),
                    ),
                    "lifecycle" => Box::new(
                        crate::session::storage::acquire_storage_flock(
                            &profile,
                            &format!(".instance-lifecycle-{id}.lock"),
                        )
                        .unwrap(),
                    ),
                    "transition" => Box::new(
                        crate::session::storage::acquire_storage_flock(
                            &namespace,
                            crate::migrations::v027_isolate_sandbox_stores::LOCK,
                        )
                        .unwrap(),
                    ),
                    "storage" => Box::new(
                        crate::session::storage::acquire_storage_flock(&profile, ".storage.lock")
                            .unwrap(),
                    ),
                    _ => unreachable!(),
                };
                ready_tx.send(()).unwrap();
                let requested = release_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .is_ok();
                drop(held);
                released_tx.send(requested).unwrap();
            });
            ready_rx.recv().unwrap();
            let before = std::fs::read(storage.sessions_path()).unwrap();
            let result = retire_provisional_stub("owner", &row.id, &provisional, &HashSet::new());
            let _ = release_tx.send(());
            let returned_while_held = released_rx.recv().unwrap();
            holder.join().unwrap();
            assert!(
                returned_while_held,
                "{layer}: cancellation waited for the holder"
            );
            assert!(result.is_err(), "{layer}");
            assert_eq!(
                std::fs::read(storage.sessions_path()).unwrap(),
                before,
                "{layer}"
            );
            retire_provisional_stub("owner", &row.id, &provisional, &HashSet::new()).unwrap();
            assert!(storage.load().unwrap().is_empty());
        }
    }
}

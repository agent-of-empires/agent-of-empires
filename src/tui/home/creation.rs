//! Creating a session: the request, its pending stub, and the cleanup a
//! cancel or a quit has to do.

use super::*;

pub(super) fn cleanup_creation_resources(
    instance: &Instance,
    created_worktree: Option<&CreatedWorktreeInfo>,
    created_workspace_worktrees: &[CreatedWorktreeInfo],
    protected_owner: Option<&Instance>,
) {
    let worktree = created_worktree.map(crate::session::builder::CreatedWorktree::from);
    let workspace_worktrees: Vec<_> = created_workspace_worktrees
        .iter()
        .map(crate::session::builder::CreatedWorktree::from)
        .collect();
    crate::session::builder::cleanup_instance(
        instance,
        worktree.as_ref(),
        &workspace_worktrees,
        protected_owner,
    );
}

pub(super) enum CreationCommit {
    Inserted,
    Duplicate(Box<Instance>),
}

/// Cross-process guards for a single-session title mutation or profile move.
/// The source profile's lifecycle flock is intentionally nested inside the
/// per-session title flock; callers retain this value through durable
/// persistence and any tmux rekey so a terminal launch cannot observe the
/// transition halfway through.
pub(in crate::tui) struct SessionMutationGuards {
    pub(super) _session_title: crate::session::StorageFlock,
    pub(super) _lifecycle: crate::session::StorageFlock,
}

impl HomeView {
    /// Request background session creation. Used for sandbox sessions to avoid blocking UI.
    /// Creates a stub instance in the session list with Status::Creating so the user
    /// can see progress in the preview pane while continuing to use the TUI.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::HooksConfig>,
    ) {
        // Pre-resolve the title using the same logic the builder will run, so the
        // stub instance, the background creation, and the eventual real instance
        // all agree on the title (otherwise an empty title would show as the path
        // basename in the stub but a civilization name in the final instance).
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

        // Set stub worktree_info so project-mode grouping works during creation.
        // The real worktree_info (with resolved main_repo_path) replaces this
        // once build_instance completes.
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
        self.add_instance(stub);
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

        self.creation_cancelled = false;
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
        };
        self.creation_poller.request_creation(request);
    }

    /// Mark the current creation operation as cancelled
    pub fn cancel_creation(&mut self) {
        if self.creation_poller.is_pending() {
            self.creation_cancelled = true;
        }
        // Remove the stub instance
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.creating_provisional_group_paths.clear();
            self.remove_instance(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
            self.rebuild_group_trees();
            self.rebuild_flat_items();
            self.update_selected();
        }
        self.new_dialog = None;
    }

    /// Apply any pending creation results from the background poller.
    /// Returns Some(session_id) if creation succeeded and we should attach.
    pub fn apply_creation_results(&mut self) -> Option<String> {
        use crate::tui::creation_poller::CreationResult;

        let result = self.creation_poller.try_recv_result()?;

        // Clean up the stub and progress tracking
        let stub_id = self.creating_stub_id.take();
        // Taken (not borrowed) so every early return below leaves the field
        // empty: the provisional group paths belong to this stub alone, and a
        // rolled-back or failed finalize must not carry them into the next
        // creation.
        let provisional_group_paths = std::mem::take(&mut self.creating_provisional_group_paths);
        if let Some(ref id) = stub_id {
            self.creating_hook_progress.remove(id);
        }

        // Check if the user cancelled while waiting
        if self.creation_cancelled {
            self.creation_cancelled = false;
            if let Some(id) = &stub_id {
                self.remove_instance(id);
            }
            if let CreationResult::Success {
                ref instance,
                ref created_worktree,
                ref created_workspace_worktrees,
                ..
            } = result
            {
                cleanup_creation_resources(
                    instance,
                    created_worktree.as_ref(),
                    created_workspace_worktrees,
                    None,
                );
            }
            self.rebuild_group_trees();
            self.rebuild_flat_items();
            self.update_selected();
            return None;
        }

        match result {
            CreationResult::Success {
                session_id,
                instance,
                created_worktree,
                created_workspace_worktrees,
                on_launch_hooks_ran,
                mut warnings,
            } => {
                // Remove the stub instance
                if let Some(id) = &stub_id {
                    self.remove_instance(id);
                }

                let mut instance = *instance;
                let target_profile = self.creation_poller.last_profile().unwrap_or_else(|| {
                    self.active_profile
                        .clone()
                        .unwrap_or_else(crate::session::config::resolve_default_profile)
                });
                instance.source_profile = target_profile.clone();

                if !self.storages.contains_key(&target_profile) {
                    match Storage::new(&target_profile, self.file_watch.clone()) {
                        Ok(storage) => {
                            self.storages.insert(target_profile.clone(), storage);
                        }
                        Err(error) => {
                            cleanup_creation_resources(
                                &instance,
                                created_worktree.as_ref(),
                                &created_workspace_worktrees,
                                None,
                            );
                            self.info_dialog = Some(InfoDialog::sized_to_fit(
                                "Creation Failed",
                                &format!("Failed to open profile storage: {error}"),
                            ));
                            self.new_dialog = None;
                            self.rebuild_group_trees();
                            self.rebuild_flat_items();
                            self.update_selected();
                            return None;
                        }
                    }
                }

                let Some(storage) = self.storages.get(&target_profile) else {
                    // The block above either found or inserted this profile's
                    // storage, so this is unreachable; bail without attaching
                    // rather than panicking on a production path.
                    return None;
                };
                let persist_result = storage.update(|instances, groups| {
                    // `save()` can run while the builder is working and persist
                    // the placeholder. Remove that exact row under the same
                    // storage lock used for collision detection and insertion,
                    // otherwise the placeholder collides with its own result.
                    let removed_persisted_stub = stub_id.as_deref().is_some_and(|stub_id| {
                        let before = instances.len();
                        instances.retain(|row| row.id != stub_id);
                        instances.len() != before
                    });
                    if removed_persisted_stub && !provisional_group_paths.is_empty() {
                        groups.retain(|group| !provisional_group_paths.contains(&group.path));
                        // A peer may have committed another row into one of
                        // these paths. Rebuild from the remaining rows so its
                        // group survives even though the stub-created metadata
                        // was provisional.
                        *groups = GroupTree::new_with_groups(instances, groups).get_all_groups();
                    }
                    if let Some(owner) = crate::session::find_duplicate_session(
                        instances.iter(),
                        &instance.title,
                        &instance.project_path,
                        None,
                    ) {
                        return Ok(CreationCommit::Duplicate(Box::new(owner.clone())));
                    }
                    instances.push(instance.clone());
                    if !instance.group_path.is_empty() {
                        let mut tree = GroupTree::new_with_groups(instances, groups);
                        tree.create_group(&instance.group_path);
                        *groups = tree.get_all_groups();
                    }
                    Ok(CreationCommit::Inserted)
                });
                match persist_result {
                    Ok(CreationCommit::Inserted) => {}
                    Ok(CreationCommit::Duplicate(owner)) => {
                        cleanup_creation_resources(
                            &instance,
                            created_worktree.as_ref(),
                            &created_workspace_worktrees,
                            Some(&owner),
                        );
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Creation Failed",
                            &crate::session::duplicate_session_error(&instance.title).to_string(),
                        ));
                        self.new_dialog = None;
                        if let Err(error) = self.reload() {
                            tracing::warn!(
                                target: "tui.home",
                                "Failed to reload authoritative state after creation collision: {error}"
                            );
                        }
                        return None;
                    }
                    Err(error) => match storage.load() {
                        Ok(instances) if instances.iter().any(|row| row.id == instance.id) => {
                            warnings.push(format!(
                                "Session metadata was written, but finalizing profile storage reported an error: {error}"
                            ));
                        }
                        Ok(instances) => {
                            let owner = crate::session::find_duplicate_session(
                                &instances,
                                &instance.title,
                                &instance.project_path,
                                None,
                            )
                            .cloned();
                            cleanup_creation_resources(
                                &instance,
                                created_worktree.as_ref(),
                                &created_workspace_worktrees,
                                owner.as_ref(),
                            );
                            self.info_dialog = Some(InfoDialog::sized_to_fit(
                                "Creation Failed",
                                &format!("Failed to save session: {error}"),
                            ));
                            self.new_dialog = None;
                            if let Err(reload_error) = self.reload() {
                                tracing::warn!(
                                    target: "tui.home",
                                    "Failed to reload authoritative state after creation rollback: {reload_error}"
                                );
                            }
                            return None;
                        }
                        Err(verify_error) => {
                            self.info_dialog = Some(InfoDialog::sized_to_fit(
                                "Creation Failed",
                                &format!(
                                    "Failed to save session and could not verify the result: {error}\n\
                                     Created resources were retained to avoid deleting a persisted session: {verify_error}"
                                ),
                            ));
                            self.new_dialog = None;
                            self.rebuild_group_trees();
                            self.rebuild_flat_items();
                            self.update_selected();
                            return None;
                        }
                    },
                }

                // `publish_persisted_instance` records the create-count and
                // clears the id from `pending_added` (the row is authoritative
                // now, not a provisional add). Its in-memory insert is
                // superseded by the `reload()` below on success, but is the
                // fallback that keeps the row visible if that reload fails.
                self.publish_persisted_instance(instance.clone());
                self.rebuild_group_trees();

                if on_launch_hooks_ran {
                    self.on_launch_hooks_ran.insert(session_id.clone());
                }

                if let Err(e) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                }
                // The creation poller may have minted `before_start_env` while
                // bringing the container up. It is `#[serde(skip)]`, so the
                // reload above dropped it; carry it back onto the live instance
                // (mirroring the CLI's `merge_post_start` and the structured-view
                // stamp-back) so the agent launch reuses it instead of re-minting.
                let minted = instance
                    .sandbox_info
                    .as_mut()
                    .map(|sb| std::mem::take(&mut sb.before_start_env))
                    .unwrap_or_default();
                if !minted.is_empty() {
                    self.mutate_instance(&session_id, |inst| {
                        if let Some(sb) = inst.sandbox_info.as_mut() {
                            sb.before_start_env = minted.clone();
                        }
                    });
                }
                // reload()'s restore-previous-selection fallback lands
                // the cursor on whichever flat_items index is closest
                // to the now-removed stub, which in project-grouped
                // layouts is often the new session's group folder.
                // Pin selection onto the new session directly so the
                // preview pane and dispatch in app.rs see the right
                // row.
                self.select_and_reveal_session(&session_id);
                self.new_dialog = None;

                if !warnings.is_empty() {
                    let body = warnings.join("\n\n");
                    let message = format!(
                        "Session was created, but the following warnings were emitted during setup:\n\n{}",
                        body
                    );
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Session warnings", &message));
                }

                Some(session_id)
            }
            CreationResult::Error(error) => {
                // Remove the stub and show the error in an info dialog
                if let Some(id) = &stub_id {
                    self.remove_instance(id);
                    self.rebuild_group_trees();
                    self.rebuild_flat_items();
                    self.update_selected();
                    // Hook failures carry multi-line output; size to fit so
                    // the actual error isn't clipped at the default 50x9.
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                } else if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_loading(false);
                    dialog.set_error(error);
                }
                None
            }
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

    /// Persist `confirm_before_quit = false` and update the cached flag so
    /// the quit confirmation stops appearing. Called when the user ticks
    /// "don't warn me again" in the quit dialog.
    pub(in crate::tui) fn disable_confirm_before_quit(&mut self) {
        self.confirm_before_quit = false;
        if let Err(e) = update_config(|config| {
            config.session.confirm_before_quit = false;
        }) {
            tracing::warn!(target: "tui.home", "Failed to save config: {e}");
        }
    }

    /// Persist the "don't warn me again" opt-out for whichever confirm
    /// offered the checkbox. Both call sites (keyboard and click) route
    /// through this so the two paths can't disagree about which confirms
    /// are opt-out-able. Actions without a checkbox never reach it.
    pub(in crate::tui) fn apply_confirm_dont_ask_again(&mut self, action: &str) {
        match action {
            "quit" => self.disable_confirm_before_quit(),
            // Written globally, matching the quit opt-out. A profile that
            // overrides confirm_delete = true keeps prompting; that override
            // is cleared from the settings pane, not from here.
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

    /// Clean up a pending creation on TUI shutdown. Waits briefly for the
    /// background thread to finish so we can clean up worktrees/instances.
    /// If the thread doesn't finish in time, the hook subprocess will
    /// complete on its own and orphaned Creating stubs are cleaned up on
    /// next launch.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.creation_poller.is_pending() {
            return;
        }
        self.creation_cancelled = true;
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.remove_instance(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
        }

        // Wait briefly for the background thread to finish
        let result = self
            .creation_poller
            .recv_result_timeout(std::time::Duration::from_secs(2));

        if let Some(crate::tui::creation_poller::CreationResult::Success {
            ref instance,
            ref created_worktree,
            ref created_workspace_worktrees,
            ..
        }) = result
        {
            cleanup_creation_resources(
                instance,
                created_worktree.as_ref(),
                created_workspace_worktrees,
                None,
            );
            tracing::info!(target: "tui.home", "Cleaned up cancelled session on exit");
        }
    }
}

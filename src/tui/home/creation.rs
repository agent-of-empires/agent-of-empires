//! Session creation, canonical publication, and retained cancellation ownership.

use super::*;
use std::path::PathBuf;

/// Cross-process guards for a single-session title mutation or profile move. The source
/// profile's lifecycle flock nests inside the per-session title flock; callers retain this
/// through durable persistence and any tmux rekey, so a terminal launch cannot observe the
/// transition halfway through.
pub(in crate::tui) struct SessionMutationGuards {
    pub(super) _session_title: crate::session::StorageFlock,
    pub(super) _lifecycle: crate::session::StorageFlock,
}

impl HomeView {
    /// Request background session creation, used for sandbox sessions so the UI does not
    /// block. A `Status::Creating` stub appears in the list, so progress shows in the
    /// preview pane while the TUI stays usable.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    ) {
        let storage = (|| -> anyhow::Result<Storage> {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            match self.storages.get(&data.profile) {
                Some(original) => original.reopen_preserving_watch(),
                None => Storage::open_or_create(&data.profile, self.file_watch.clone()),
            }
        })();
        let storage = match storage {
            Ok(storage) => std::sync::Arc::new(storage),
            Err(error) => {
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Creation Failed",
                    &format!("{error:#}"),
                ));
                return;
            }
        };
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
        let admitted_instance = Instance::new(&stub_title, &data.path);
        let mut stub = admitted_instance.clone();
        stub.tool = if data.tool.is_empty() {
            "claude".to_string()
        } else {
            data.tool.clone()
        };
        stub.group_path = data.group.clone();
        stub.status = crate::session::Status::Creating;
        stub.yolo_mode = data.yolo_mode;
        stub.source_profile = data.profile.clone();

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
            storage,
            admitted_instance,
            data,
            existing_instances,
            hooks,
            cancel,
        };
        self.creation_poller.request_creation(request);
    }

    fn remove_creation_stub(&mut self, id: &str) {
        if let Some(instance) = self.instances.shift_remove(id) {
            if let Some(pending) = self.pending_added.get_mut(&instance.source_profile) {
                pending.remove(id);
            }
        }
    }
    /// Cancel at the next worker boundary; unproven ownership remains retained.
    pub fn cancel_creation(&mut self) {
        if let Some(cancel) = self.creation_cancel.take() {
            cancel.cancel();
        }
        // Remove the stub instance
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.creating_provisional_group_paths.clear();
            self.remove_creation_stub(&stub_id);
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

        let outcome = self.creation_poller.try_recv_result()?;
        let result = outcome.result;

        // A cancelled request's stub is already gone; the fields below may belong to a
        // newer request, so leave them alone.
        if outcome.cancelled || matches!(result, CreationResult::Cancelled) {
            if let CreationResult::Success { ref instance, .. } = result {
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Cancelled creation retained",
                    &format!("Session {} and its resources at {} are retained because original owner quiescence is unproven.", instance.id, instance.project_path),
                ));
            } else if let CreationResult::Error(ref error) = result {
                self.info_dialog = Some(InfoDialog::sized_to_fit("Cancelled creation", error));
            }
            return None;
        }

        self.creation_cancel = None;
        let stub_id = self.creating_stub_id.take();
        // Taken, not borrowed, so every early return leaves the field empty: the
        // provisional group paths belong to this stub alone and must not carry into the
        // next creation.
        self.creating_provisional_group_paths.clear();
        if let Some(ref id) = stub_id {
            self.creating_hook_progress.remove(id);
        }

        match result {
            CreationResult::Success {
                session_id,
                instance,
                creation_intent,
                on_launch_hooks_ran,
                warnings,
            } => {
                let mut instance = *instance;
                // Taken here rather than carried over the channel from the
                // builder thread: the UI thread must be able to take these
                // flocks itself (save, reload and the publish path all need
                // them), so a guard owned by the worker would self-deadlock.
                // Workspace claim before identity, the single order every other
                // owner uses.
                let ownership_locks = match crate::session::builder::CleanupOwnershipLocks::acquire(
                ) {
                    Ok(locks) => locks,
                    Err(error) => {
                        tracing::warn!(target: "tui.create", "Creation ownership and resources retained: original native quiescence is unproven");
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Creation Failed",
                            &format!("Could not lock the session inventory to publish: {error}"),
                        ));
                        self.new_dialog = None;
                        self.rebuild_group_trees();
                        self.rebuild_flat_items();
                        self.update_selected();
                        return None;
                    }
                };

                // Remove the stub instance
                if let Some(id) = &stub_id {
                    self.remove_creation_stub(id);
                }

                let storage = &*outcome.storage;
                if let Err(error) = storage.verify_profile_identity() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Creation Failed",
                        &format!(
                            "Original profile was replaced; retaining created resources: {error:#}"
                        ),
                    ));
                    self.new_dialog = None;
                    self.rebuild_group_trees();
                    self.rebuild_flat_items();
                    self.update_selected();
                    return None;
                }
                let manages_worktree = instance
                    .worktree_info
                    .as_ref()
                    .is_some_and(|worktree| worktree.managed_by_aoe)
                    || instance.workspace_info.is_some();
                let authoritative = match storage.load() {
                    Ok(authoritative) => authoritative,
                    Err(error) => {
                        tracing::warn!(target: "tui.create", "Creation ownership and resources retained: original native quiescence is unproven");
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Creation Failed",
                            &format!("Failed to read profile storage: {error}"),
                        ));
                        self.new_dialog = None;
                        // `reload()` re-takes the identity lock for duplicate
                        // reconciliation, so the ownership flocks go first.
                        drop(ownership_locks);
                        let _ = self.reload();
                        return None;
                    }
                };
                if let Err(error) = crate::session::validate_managed_workspace(&instance) {
                    tracing::warn!(target: "tui.create", "Creation ownership and resources retained: original native quiescence is unproven");
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Creation Failed",
                        &format!("Managed workspace validation failed: {error}"),
                    ));
                    self.new_dialog = None;
                    drop(ownership_locks);
                    let _ = self.reload();
                    return None;
                }
                if manages_worktree
                    && crate::session::find_duplicate_session(
                        authoritative.iter(),
                        &instance.title,
                        &instance.project_path,
                        None,
                    )
                    .is_none()
                {
                    let mut candidate_paths = vec![PathBuf::from(&instance.project_path)];
                    candidate_paths.extend(
                        instance
                            .all_repos()
                            .iter()
                            .map(|repo| PathBuf::from(&repo.worktree_path)),
                    );
                    if let Err(error) = crate::session::deletion::ensure_unclaimed_paths(
                        crate::session::deletion::SessionPathOwner {
                            profile: storage.profile(),
                            session_id: &instance.id,
                        },
                        &candidate_paths,
                    ) {
                        tracing::warn!(target: "tui.create", "Creation ownership and resources retained: original native quiescence is unproven");
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Creation Failed",
                            &format!("Session path is already claimed: {error}"),
                        ));
                        self.new_dialog = None;
                        drop(ownership_locks);
                        let _ = self.reload();
                        return None;
                    }
                }
                let persist_result =
                    crate::session::builder::publish_prepared_creation_under_workspace_claim_lock(
                        storage,
                        &instance,
                        &creation_intent,
                        |instances, groups| {
                            if !instance.group_path.is_empty() {
                                let mut tree = GroupTree::new_with_groups(instances, groups);
                                tree.create_group(&instance.group_path);
                                *groups = tree.get_all_groups();
                            }
                            Ok(())
                        },
                    );
                match persist_result {
                    Ok(committed) => instance = committed,
                    Err(error) => {
                        self.info_dialog = Some(InfoDialog::sized_to_fit(
                            "Creation Failed",
                            &format!(
                                "Creation publication failed; resources were retained: {error}"
                            ),
                        ));
                        self.new_dialog = None;
                        drop(ownership_locks);
                        if let Err(reload_error) = self.reload() {
                            tracing::warn!(target: "tui.home", "Could not reload failed creation: {reload_error}");
                        }
                        return None;
                    }
                }

                // Publish locally only after the actual canonical acknowledgement.
                self.publish_persisted_instance(instance.clone());
                self.rebuild_group_trees();

                if on_launch_hooks_ran {
                    self.on_launch_hooks_ran.insert(session_id.clone());
                }
                drop(ownership_locks);

                if let Err(e) = self.reload() {
                    tracing::warn!(target: "tui.home", "Failed to reload session state: {e}");
                }
                // The creation poller may have minted `before_start_env` while bringing the
                // container up. It is `#[serde(skip)]`, so the reload dropped it; carry it
                // back onto the live instance (as the CLI's `merge_post_start` does) so the
                // agent launch reuses it instead of re-minting.
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
                // reload()'s restore-previous-selection fallback lands the cursor on
                // whichever index is closest to the removed stub, often the new session's
                // group folder, so pin the selection onto the new session directly.
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
                    self.remove_creation_stub(id);
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

    /// Clean up a pending creation on shutdown, waiting briefly for the background thread
    /// so worktrees and instances can be cleaned up. If it does not finish in time the hook
    /// subprocess completes on its own and orphaned Creating stubs are cleaned up on the
    /// next launch.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.creation_poller.is_pending() {
            return;
        }
        if let Some(cancel) = self.creation_cancel.take() {
            cancel.cancel();
        }
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.remove_creation_stub(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
        }

        // Receive completed requests without discarding their retention decision.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.creation_poller.is_pending() {
            let Some(outcome) = self
                .creation_poller
                .recv_result_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            else {
                break;
            };
            if let crate::tui::creation_poller::CreationResult::Success { ref instance, .. } =
                outcome.result
            {
                tracing::warn!(target: "tui.home", session_id = %instance.id, "Cancelled creation ownership remains retained on exit");
            }
        }
    }
}

//! Session creation, canonical publication, and retained cancellation ownership.

use super::*;
use crate::session::Status;

impl HomeView {
    /// Request background session creation, used for sandbox sessions so the UI does not
    /// block. A `Status::Creating` stub appears in the list, so progress shows in the
    /// preview pane while the TUI stays usable.
    pub fn request_creation(
        &mut self,
        mut data: NewSessionData,
        hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    ) {
        if data.profile.is_empty() {
            data.profile = crate::session::config::resolve_default_profile();
        }
        let storage = if let Some(original) = self.storages.get(&data.profile) {
            Some(original.clone())
        } else {
            let captured = (|| {
                let path = crate::session::get_profile_dir_path(&data.profile)?;
                if path.try_exists()? {
                    Storage::open(&data.profile, self.file_watch.clone()).map(Some)
                } else {
                    Ok(None)
                }
            })();
            match captured {
                Ok(storage) => storage,
                Err(error) => {
                    self.info_dialog=Some(InfoDialog::new("Creation Failed",&format!("Original creation profile could not be captured before admission: {error:#}")));
                    return;
                }
            }
        };
        self.creating_provisional_profile = Some(data.profile.clone());
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
        let mut admitted_instance = Instance::new(&stub_title, &data.path);
        admitted_instance.source_profile = data.profile.clone();
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
        let request = persistence_transactions::CreationAdmission {
            admitted_instance,
            data,
            existing_instances,
            hooks,
            cancel,
        };
        if let Err(error) = self.request_transaction(
            persistence_transactions::TransactionRequest::AdmitCreation {
                storage,
                request: Box::new(request),
            },
        ) {
            self.info_dialog = Some(InfoDialog::new("Creation Failed", &format!("{error:#}")));
        }
    }

    pub(super) fn remove_creation_stub(&mut self, id: &str) {
        if self
            .instances
            .get(id)
            .is_some_and(|row| row.status == Status::Creating && row.lifecycle_generation > 0)
        {
            return;
        }
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
            self.creating_provisional_profile = None;
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
    pub(in crate::tui) fn apply_creation_results(&mut self) -> Option<CreatedContinuation> {
        use crate::tui::creation_poller::CreationResult;

        if let Some(id) = self.persistence.created.pop_front() {
            return Some(id);
        }
        let outcome = self.creation_poller.try_recv_result()?;
        let original_id = outcome.session_id;
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

        let stub_id = self
            .creating_stub_id
            .as_ref()
            .filter(|id| **id == original_id)
            .cloned();
        // Keep this original ID and provisional metadata until publication ACK;
        // native preparation is not durable publication. A later request owns its own marker.
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
                let Some(custody) = crate::session::builder::CreationCustody::retained()
                    .into_iter()
                    .find(|custody| {
                        custody.session_id() == session_id
                            && custody.created_at() == instance.created_at
                            && custody.storage().same_origin_as(&outcome.storage)
                    })
                else {
                    self.info_dialog=Some(InfoDialog::new("Original creation custody unavailable","The actual original custodian and native receipts are missing. A result DTO cannot reconstruct them; publication was not attempted."));
                    return None;
                };
                // The native worker retained the complete prepared result before send/cancel.
                // Only the retained original may publish; a DTO or a profile-name reopen is not authority.
                let _ = creation_intent;
                let _ = (on_launch_hooks_ran, warnings);
                if let Some(ref id) = stub_id {
                    self.remove_creation_stub(id);
                }
                if stub_id.is_some() || self.creating_stub_id.is_none() {
                    self.creating_stub_id = Some(session_id);
                }
                if let Err(error) = self.request_transaction(
                    persistence_transactions::TransactionRequest::PublishCreation {
                        custody,
                        cancel: outcome.cancel,
                    },
                ) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Creation Failed",
                        &format!("Original creation retained: {error:#}"),
                    ));
                }
                None
            }
            CreationResult::Error(error) => {
                if stub_id.is_some() {
                    self.creation_cancel = None;
                    self.creating_stub_id = None;
                    self.creating_provisional_group_paths.clear();
                    self.creating_provisional_profile = None;
                }
                self.request_reload(ReloadKind::Full);
                // Remove the stub and show the error in an info dialog
                if let Some(id) = &stub_id {
                    self.remove_creation_stub(id);
                    self.rebuild_group_trees();
                    self.rebuild_flat_items();
                    self.update_selected();
                    // Hook failures carry multi-line output; size to fit so
                    // the actual error isn't clipped at the default 50x9.
                    self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                } else {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Original creation failed",
                        &format!("{original_id}: {error}"),
                    ));
                }
                None
            }
            // Returned early above.
            CreationResult::Cancelled => None,
        }
    }

    /// Check if on_launch hooks already ran for this session (and consume the flag).
    pub fn take_on_launch_hooks_ran(&mut self, session_id: &str) -> bool {
        self.on_launch_hooks_ran
            .remove(session_id)
            .is_some_and(|origin| {
                self.get_instance(session_id)
                    .is_some_and(|row| origin.matches(row))
            })
    }

    /// Check if there's a pending creation operation
    pub fn is_creation_pending(&self) -> bool {
        self.creation_poller.is_pending() || self.creating_stub_id.is_some()
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

    /// Queue the quit opt-out; the cached flag changes only after its durable ACK.
    pub(in crate::tui) fn disable_confirm_before_quit(&mut self) {
        if let Err(error) = self.enqueue_transaction(
            persistence_transactions::TransactionRequest::DisableQuitConfirmation,
            super::persistence_worker::SaveSnapshot {
                profiles: Vec::new(),
            },
        ) {
            self.cancel_persistence_quit(format!("Quit preference was not admitted: {error:#}"));
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

    /// Cancel publication without discarding original creation ownership.
    pub fn cleanup_pending_creation(&mut self) {
        if !self.is_creation_pending() {
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

impl HomeView {
    pub(super) fn prompt_creation_recovery(
        &mut self,
        action: persistence_transactions::CreationRecoveryAction,
    ) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let result = (|| {
            let row = self.capture_transaction_row(&id)?;
            anyhow::ensure!(
                row.before.status == Status::Creating,
                "Only a Creating row can resolve its retained original creation"
            );
            let cancel = (self.creating_stub_id.as_deref() == Some(id.as_str()))
                .then(|| self.creation_cancel.clone())
                .flatten();
            self.request_transaction(
                persistence_transactions::TransactionRequest::ResolveCreation {
                    row,
                    action,
                    cancel,
                },
            )
        })();
        if let Err(error) = result {
            self.info_dialog = Some(InfoDialog::new(
                "Creation recovery unavailable",
                &format!("{error:#}"),
            ));
        }
    }
    pub(super) fn prompt_claim_abort(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let result = self.capture_transaction_row(&id).and_then(|row| {
            self.request_transaction(
                persistence_transactions::TransactionRequest::PrepareClaimAbort(row),
            )
        });
        if let Err(error) = result {
            self.info_dialog = Some(InfoDialog::new(
                "Intent metadata abort unavailable",
                &format!("{error:#}"),
            ));
        }
    }
    pub(super) fn submit_creation_confirmation(&mut self) {
        if let Some(selection) = self.pending_claim_abort_confirmation.take() {
            if let Err(error) = self.request_transaction(
                persistence_transactions::TransactionRequest::AbortClaim(selection),
            ) {
                self.info_dialog = Some(InfoDialog::new(
                    "Intent metadata abort failed",
                    &format!("{error:#}"),
                ));
            }
            return;
        }
        let Some(confirmation) = self.pending_creation_confirmation.take() else {
            return;
        };
        if let Err(error) = self.request_transaction(
            persistence_transactions::TransactionRequest::RecoverCreation(confirmation),
        ) {
            self.info_dialog = Some(InfoDialog::new(
                "Creation recovery failed",
                &format!("{error:#}"),
            ));
        }
    }
}

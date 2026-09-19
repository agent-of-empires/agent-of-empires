//! Creating a session: the request, its pending stub, and the cleanup a
//! cancel or a quit has to do.

use super::*;

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
    /// Request session creation from the daemon. The stub is a display-only
    /// placeholder: the daemon provisions, runs the hooks, and commits the row,
    /// which then lands through the canonical feed.
    pub fn request_creation(&mut self, mut data: NewSessionData, trust_hooks: Option<bool>) {
        if self.pending_creation.is_some() {
            self.flash_status("A session is already being created");
            return;
        }
        // The daemon owns the creation, so a disconnected runtime must refuse
        // before a placeholder claims a row that nothing will ever commit.
        if !self.session_feed.mutations_available() {
            self.new_dialog = None;
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Cannot Create Session",
                "The local daemon is unavailable, so the session was not created.",
            ));
            return;
        }
        // Pre-resolve the title with the same logic the daemon's builder runs,
        // so the placeholder and the committed row agree (otherwise an empty
        // title shows as the path basename here and a civilization name there).
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
        // The committed row replaces it once the daemon publishes the session.
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
        self.creating_stub_id = Some(stub_id.clone());
        self.instances.insert(stub_id.clone(), stub);
        self.rebuild_group_trees();
        self.creating_hook_progress.insert(
            stub_id.clone(),
            CreatingHookProgress {
                hook_output: Vec::new(),
                current_hook: None,
            },
        );
        self.rebuild_flat_items();
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &stub_id))
        {
            self.cursor = pos;
            self.update_selected();
        }
        self.new_dialog = None;

        let body = wizard_create_body(&data, trust_hooks);
        if let Err(error) = self.session_feed.create_session(stub_id.clone(), body) {
            self.pending_creation = None;
            self.discard_creating_stub();
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Creation Failed",
                &error.to_string(),
            ));
            return;
        }
        self.pending_creation = Some(PendingCreation {
            daemon_id: None,
            title: stub_title,
            profile: target_profile,
            cancel_requested: false,
        });
    }

    /// Bind a pending creation to the daemon's own row and keep that row out of
    /// the sidebar. Runs on every applied snapshot: the reservation is published
    /// before provisioning, so this binds the id without waiting for a progress
    /// frame, and re-hides the row if a reload brought it back in. Returns
    /// whether anything changed.
    pub(super) fn reconcile_in_flight_creation(
        &mut self,
        rows: &[crate::daemon::SessionResponse],
    ) -> bool {
        let id = {
            let Some(pending) = self.pending_creation.as_mut() else {
                return false;
            };
            if let Some(id) = pending.daemon_id.clone() {
                id
            } else {
                let Some(row) = rows
                    .iter()
                    .find(|row| row.title == pending.title && row.profile == pending.profile)
                else {
                    return false;
                };
                pending.daemon_id = Some(row.id.clone());
                row.id.clone()
            }
        };
        if self.instances.contains_key(&id) {
            self.hide_in_flight_reservation(&id);
            return true;
        }
        false
    }

    /// The daemon's session id for the creation this view is displaying.
    pub(super) fn in_flight_creation_id(&self) -> Option<&str> {
        self.pending_creation.as_ref()?.daemon_id.as_deref()
    }

    /// Drop the in-flight creation's own row from the sidebar. Deliberately not
    /// `remove_instance`: that records a deletion to persist, and this row is the
    /// daemon's, not one this view owns.
    fn hide_in_flight_reservation(&mut self, id: &str) {
        if self.instances.shift_remove(id).is_some() {
            self.rebuild_flat_items();
            match self.creating_stub_id.clone() {
                Some(stub) => self.select_session_by_id(&stub),
                None => self.update_selected(),
            }
        }
    }

    /// The token the feed keyed this creation under: the placeholder's id while
    /// it is displayed, or the daemon's id once the placeholder is gone.
    fn creating_token(&self) -> Option<&str> {
        self.creating_stub_id
            .as_deref()
            .or_else(|| self.pending_creation.as_ref()?.daemon_id.as_deref())
    }

    /// Remove the creating placeholder and its buffered progress. A creation
    /// whose id the daemon has not named yet stays pending, so its
    /// cancellation is still delivered once the id is known.
    fn discard_creating_stub(&mut self) {
        if let Some(stub_id) = self.creating_stub_id.take() {
            self.instances.shift_remove(&stub_id);
            self.creating_hook_progress.remove(&stub_id);
        }
        self.rebuild_group_trees();
        self.rebuild_flat_items();
        self.update_selected();
    }

    /// Ask the daemon to cancel the in-flight creation. The daemon finishes the
    /// phase in flight before rolling back, so the placeholder goes away now and
    /// no row appears unless the cancel arrived too late.
    pub fn cancel_creation(&mut self) {
        let Some(pending) = self.pending_creation.as_mut() else {
            self.new_dialog = None;
            return;
        };
        pending.cancel_requested = true;
        if let Some(id) = pending.daemon_id.clone() {
            if let Err(error) = self.session_feed.cancel_creation(id) {
                tracing::warn!(target: "tui.home", %error, "creation cancellation was refused");
            }
        }
        self.discard_creating_stub();
        self.new_dialog = None;
    }

    /// Bind the placeholder to the daemon's id and mirror the daemon's progress
    /// into its preview buffer. Idempotent: progress repeats until it settles.
    pub(super) fn apply_creation_progress(&mut self) -> bool {
        let Some(progress) = self.session_feed.drain_progress() else {
            return false;
        };
        let stub_id = self.creating_stub_id.clone();
        let mut changed = false;
        let mut deferred_cancel = None;
        let mut bound = None;
        {
            let Some(pending) = self.pending_creation.as_mut() else {
                return false;
            };
            let Some(entry) = progress.iter().find(|entry| match &pending.daemon_id {
                Some(id) => entry.session_id == *id,
                None => entry.title == pending.title && entry.profile == pending.profile,
            }) else {
                return false;
            };
            if pending.daemon_id.as_deref() != Some(entry.session_id.as_str()) {
                pending.daemon_id = Some(entry.session_id.clone());
                changed = true;
                bound = Some(entry.session_id.clone());
            }
            if pending.cancel_requested {
                deferred_cancel = Some(entry.session_id.clone());
            }
            if let Some(buffer) = stub_id
                .as_ref()
                .and_then(|id| self.creating_hook_progress.get_mut(id))
            {
                let current = match &entry.command {
                    Some(command) => format!("{}: {}", phase_label(entry.phase), command),
                    None => phase_label(entry.phase).to_string(),
                };
                if buffer.current_hook.as_deref() != Some(current.as_str()) {
                    buffer.current_hook = Some(current);
                    changed = true;
                }
                if buffer.hook_output != entry.output {
                    buffer.hook_output.clone_from(&entry.output);
                    changed = true;
                }
            }
        }
        // The daemon's own reservation for this creation is now addressable, so
        // keep it out of the sidebar: the placeholder already represents it, and
        // two rows for one creation would double-render it.
        if let Some(id) = bound {
            self.hide_in_flight_reservation(&id);
        }
        // A cancellation asked for before the daemon named the creation lands
        // here, once it can be addressed.
        if let Some(id) = deferred_cancel {
            if let Err(error) = self.session_feed.cancel_creation(id) {
                tracing::warn!(target: "tui.home", %error, "creation cancellation was refused");
            }
        }
        changed
    }

    /// Apply any pending creation results from the daemon.
    /// Returns Some(session_id) if creation succeeded and we should attach.
    pub fn apply_creation_results(&mut self) -> Option<String> {
        let settled = self.session_feed.drain_creation_results();
        let (stub_id, result) = settled
            .into_iter()
            .find(|(token, _)| self.creating_token() == Some(token.as_str()))?;
        let cancelled = self
            .pending_creation
            .as_ref()
            .is_some_and(|pending| pending.cancel_requested);
        self.creating_stub_id = None;
        self.pending_creation = None;
        self.instances.shift_remove(&stub_id);
        self.creating_hook_progress.remove(&stub_id);
        if cancelled {
            return match result {
                // Cancelled while the daemon still owned the creation: nothing
                // was committed, so the placeholder is all that is left to drop.
                Err(_) => {
                    self.flash_status("Creation cancelled");
                    None
                }
                // The daemon had already committed when the cancellation
                // arrived, so the session stands and the user keeps it.
                Ok(_) => {
                    self.flash_status("Creation had already committed; cancellation was too late");
                    self.reload().ok();
                    None
                }
            };
        }
        match result {
            Ok(receipt) => {
                let session_id = receipt.outcome.id.clone();
                crate::tui::app::record_session_create();
                let warnings = receipt.outcome.warnings.clone();
                // The daemon committed before answering; load the row it
                // published instead of rebuilding one from the response.
                if let Err(error) = self.reload() {
                    tracing::warn!(target: "tui.home", "reload after creation failed: {error}");
                }
                self.select_and_reveal_session(&session_id);
                self.new_dialog = None;
                if !warnings.is_empty() {
                    self.info_dialog = Some(InfoDialog::sized_to_fit(
                        "Session warnings",
                        &format!(
                            "Session was created, but the following warnings were emitted during setup:\n\n{}",
                            warnings.join("\n\n")
                        ),
                    ));
                }
                Some(session_id)
            }
            Err(error) => {
                self.rebuild_group_trees();
                self.rebuild_flat_items();
                self.update_selected();
                // Hook failures carry multi-line output; size to fit so the
                // actual error is not clipped at the default 50x9.
                self.info_dialog = Some(InfoDialog::sized_to_fit("Creation Failed", &error));
                None
            }
        }
    }

    /// Clear the placeholder on quit. The daemon keeps admitted work, so this
    /// only asks for cancellation when the creation already has an id.
    pub(in crate::tui) fn cleanup_pending_creation(&mut self) {
        let Some(pending) = self.pending_creation.as_ref() else {
            return;
        };
        if let Some(id) = pending.daemon_id.clone() {
            if let Err(error) = self.session_feed.cancel_creation(id) {
                tracing::warn!(target: "tui.home", %error, "creation cancellation at quit was refused");
            }
        }
        self.discard_creating_stub();
    }

    /// Check if there's a pending creation operation
    pub fn is_creation_pending(&self) -> bool {
        self.pending_creation.is_some()
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
}

/// Wizard output as the daemon's create request. `trust_hooks` carries the
/// wizard's own decision: `Some(true)` once the user approved the repository's
/// hooks (the daemon then persists that approval before provisioning),
/// `Some(false)` for a deliberate skip, `None` when there is nothing to approve.
fn wizard_create_body(
    data: &NewSessionData,
    trust_hooks: Option<bool>,
) -> crate::daemon::CreateSessionBody {
    crate::daemon::CreateSessionBody {
        title: Some(data.title.clone()),
        size: None,
        path: data.path.clone(),
        tool: data.tool.clone(),
        group: data.group.clone(),
        yolo_mode: data.yolo_mode,
        worktree_enabled: data.worktree_enabled,
        worktree_branch: data.worktree_branch.clone(),
        create_new_branch: data.create_new_branch,
        base_branch: data.base_branch.clone(),
        sandbox: data.sandbox,
        extra_args: data.extra_args.clone(),
        sandbox_image: (!data.sandbox_image.is_empty()).then(|| data.sandbox_image.clone()),
        extra_env: data.extra_env.clone(),
        extra_repo_paths: data.extra_repo_paths.clone(),
        // The dialog collects one base for the whole session; per-repo bases
        // are a CLI and web-wizard input for now (#3329).
        repo_bases: Vec::new(),
        command_override: data.command_override.clone(),
        custom_instruction: None,
        profile: Some(data.profile.clone()),
        view: if data.structured {
            crate::session::View::Structured
        } else {
            crate::session::View::Terminal
        },
        agent_name: None,
        agent_model: None,
        agent_effort: None,
        scratch: data.scratch,
        trust_hooks,
        trust_review: None,
        import_acp_session_id: None,
        // The wizard resolved the parent's provider conversation id; the daemon
        // rebuilds the seed from it and re-checks fork capability, choosing the
        // structured or terminal shape from the requested view.
        fork_from: match &data.fork_seed {
            Some(crate::session::ForkSeed::Structured {
                parent_acp_session_id,
            }) => Some(parent_acp_session_id.clone()),
            Some(crate::session::ForkSeed::Terminal {
                parent_agent_session_id,
                ..
            }) => Some(parent_agent_session_id.clone()),
            None => None,
        },
        fork_session_id: None,
        callback_url: None,
        idempotency_key: None,
    }
}

/// Human label for a creation phase, shown beside the running hook command.
fn phase_label(phase: crate::daemon::CreationPhase) -> &'static str {
    use crate::daemon::CreationPhase;
    match phase {
        CreationPhase::Reserving => "Reserving",
        CreationPhase::Provisioning => "Preparing",
        CreationPhase::CreateHooks => "Running setup hooks",
        CreationPhase::LaunchHooks => "Running launch hooks",
    }
}

impl HomeView {
    /// Place the cursor for an archive toggle once the snapshot shows the row
    /// actually sank or rose. Returns whether the placement ran.
    pub(super) fn apply_pending_archive_cursor(&mut self) -> bool {
        let Some(pending) = self.pending_archive_cursor.take() else {
            return false;
        };
        let settled = self
            .instances
            .get(&pending.id)
            .is_some_and(|inst| inst.is_archived() == pending.archived);
        if !settled {
            // The snapshot predates the change; the next one carries it.
            self.pending_archive_cursor = Some(pending);
            return false;
        }
        self.rebuild_flat_items();
        if !pending.archived {
            self.select_session_by_id(&pending.id);
            if self.selected_session.as_deref() != Some(pending.id.as_str()) {
                // A reload frame can move the cursor off the row before it is
                // visible again; keep the intent and settle on the next
                // snapshot so the preview follows the row back.
                self.pending_archive_cursor = Some(pending);
                return false;
            }
            return true;
        }
        if self.sort_order == crate::session::config::SortOrder::Attention {
            // Attention sort is a triage flow: the cursor advances to the next
            // item that needs attention.
            self.select_top_attention(None);
            if self.selected_session.as_deref() == Some(pending.id.as_str()) {
                self.clear_selection_off_archived_row();
            }
            return true;
        }
        // Advance to the next session rather than following the archived row into
        // the Archived section: archiving reads as "I'm done with this one".
        match pending.successor {
            Some(next) => {
                self.select_session_by_id(&next);
            }
            None => self.clear_selection_off_archived_row(),
        }
        true
    }

    /// Leave no selection on a row that just sank into the Archived section: a
    /// session row already parked there must not become the selection, and the
    /// heading itself resolves to nothing.
    fn clear_selection_off_archived_row(&mut self) {
        self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
        let live = match self.flat_items.get(self.cursor) {
            Some(Item::Session { id, .. }) => self
                .instances
                .get(id)
                .is_some_and(|inst| !inst.is_archived()),
            _ => false,
        };
        if live {
            self.update_selected();
        } else {
            self.selected_session = None;
        }
    }
}

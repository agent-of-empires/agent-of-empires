//! Session operations for HomeView (create, delete, rename)

use crate::session::{
    acquire_session_identity_lock, acquire_session_workspace_claim_lock, Instance, Item,
    LifecycleOperation, StartBlocked, Status, Storage,
};
use crate::tui::deletion_poller::DeletionRequest;
use crate::tui::dialogs::{DeleteOptions, GroupDeleteOptions, InfoDialog};

use super::{persistence_transactions, HomeView};

/// Matches instances whose `group_path` is `group_path` or nests beneath it,
/// optionally scoped to one profile. `prefix` must be `"{group_path}/"`; callers
/// already have it computed.
fn group_membership<'a>(
    group_path: &'a str,
    prefix: &'a str,
    profile: Option<&'a str>,
) -> impl Fn(&Instance) -> bool + 'a {
    move |i: &Instance| {
        (i.group_path == group_path || i.group_path.starts_with(prefix))
            && profile.is_none_or(|p| i.source_profile == p)
    }
}

pub(super) fn rekey_tmux_after_persist(
    id: &str,
    old_title: &str,
    new_title: &str,
    target: anyhow::Result<Option<crate::tmux::Session>>,
) -> Option<String> {
    if old_title == new_title {
        return None;
    }
    match crate::tmux::rekey_session(id, new_title, target) {
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(target: "tui.home", session = %id, "tmux rename failed after persistence: {error}");
            Some(format!(
                "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
            ))
        }
    }
}

/// Compact snooze label (`"30 min"`, `"1 hr"`, `"2 hr 30 min"`), general for any
/// value even though the picker only submits 30 / 60 / 1440.
fn humanize_minutes(m: u32) -> String {
    let hours = m / 60;
    let mins = m % 60;
    match (hours, mins) {
        (0, _) => format!("{} min", mins),
        (_, 0) => format!("{} hr", hours),
        _ => format!("{} hr {} min", hours, mins),
    }
}

/// Why a tied-worktree rename must refuse to move the worktree directory: the
/// `rename(2)` behind `git worktree move` fails while an agent or a sandbox
/// container (alive even when Idle) holds the dir. Stopping the session clears both.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorktreeRenameBlock {
    /// The session's agent is busy (running, starting, etc.).
    ActiveAgent,
    /// A sandbox container is running and mounting the worktree dir.
    SandboxContainer,
}

/// Whether the move must be blocked, and why; `None` when it is safe. Status takes
/// precedence over the container reason.
pub(super) fn worktree_rename_block(
    status: Status,
    is_sandboxed: bool,
    container_running: bool,
) -> Option<WorktreeRenameBlock> {
    if status.blocks_worktree_edit() {
        Some(WorktreeRenameBlock::ActiveAgent)
    } else if is_sandboxed && container_running {
        Some(WorktreeRenameBlock::SandboxContainer)
    } else {
        None
    }
}

pub(super) fn worktree_rename_block_message(reason: &WorktreeRenameBlock) -> &'static str {
    match reason {
        WorktreeRenameBlock::ActiveAgent => "This worktree session's directory moves to match the new name, which can't happen while it's running. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
        WorktreeRenameBlock::SandboxContainer => "This sandbox session's container is mounting the worktree directory, so it can't be moved to match the new name. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
    }
}

impl HomeView {
    /// Pin or unpin the project header under the cursor (project view only).
    ///
    /// Pinning registers the repo if needed and sets `pinned`. Unpinning clears the flag
    /// but keeps the registry entry, so the project stays saved and only loses its header
    /// once it has no sessions; only the projects dialog removes an entry. Goes through
    /// `projects::add` / `projects::set_pinned`, the same path the web API uses. See #2208.
    pub(super) fn toggle_project_pin_at_cursor(&mut self) {
        use crate::session::{projects, Project, ProjectScope};
        use crate::tui::dialogs::InfoDialog;

        let Some(label) = self.project_group_at_cursor() else {
            return;
        };
        let profile = self.config_profile();
        // The header's own canonical repo path, or None for an empty pinned header.
        // Keying on it keeps two repos that share a basename independent.
        let header_path = self.project_header_repo_path(&label);

        if self.is_project_label_pinned(&label) {
            // Unpin the entry whose canonical path matches the header's repo. An empty
            // header has no session path, so fall back to the basename match.
            let existing = match &header_path {
                Some(path) => self
                    .registered_projects
                    .iter()
                    .find(|p| projects::canonical_key(&p.path) == *path),
                None => self
                    .registered_projects
                    .iter()
                    .find(|p| projects::repo_label(&p.path) == label),
            }
            .cloned();
            let Some(existing) = existing else {
                return;
            };
            let target = existing.path.clone();
            // On success stay quiet: the header's pin icon flips, which is
            // feedback enough. Only surface a dialog when the toggle fails.
            if let Err(e) = self.set_project_pinned_all_scopes(&target, &profile, false) {
                self.info_dialog = Some(InfoDialog::new(
                    "Unpin Failed",
                    &format!("Could not unpin: {}", e),
                ));
            }
        } else {
            // Pin the repo backing this header; an unpinned header always has a live
            // session, so its path is known. Flip an already-saved entry, else register.
            let Some(repo_path) = header_path else {
                return;
            };
            let already_registered = self
                .registered_projects
                .iter()
                .any(|p| projects::canonical_key(&p.path) == repo_path);
            let result = if already_registered {
                self.set_project_pinned_all_scopes(&repo_path, &profile, true)
            } else {
                projects::add(
                    &profile,
                    ProjectScope::Global,
                    Project::new(label.clone(), repo_path, ProjectScope::Global).with_pinned(true),
                    false,
                )
                .map(|_| ())
            };
            // On success stay quiet: the header's pin icon appears, which is
            // feedback enough. Only surface a dialog when the toggle fails.
            if let Err(e) = result {
                self.info_dialog = Some(InfoDialog::new(
                    "Pin Failed",
                    &format!("Could not pin: {}", e),
                ));
            }
        }

        self.refresh_registered_projects();
        self.rebuild_flat_items();
        self.update_selected();
    }

    /// Set `pinned` on every registry entry for `target_path` across the global file and
    /// every profile: a path can be registered in several scopes at once and the visible
    /// entry does not say which. Per-scope `NotFound` is ignored, a real I/O failure is
    /// surfaced, and no match anywhere is `NotFound`. See #2208.
    fn set_project_pinned_all_scopes(
        &self,
        target_path: &str,
        profile: &str,
        pinned: bool,
    ) -> Result<(), crate::session::projects::RegistryError> {
        use crate::session::{projects, ProjectScope};
        let mut profiles: Vec<String> = self.storages.keys().cloned().collect();
        if !profiles.iter().any(|p| p == profile) {
            profiles.push(profile.to_string());
        }
        // Global lives in one shared file, so the profile arg is irrelevant.
        let mut updates = vec![projects::set_pinned(
            profile,
            ProjectScope::Global,
            target_path,
            pinned,
        )];
        for p in &profiles {
            updates.push(projects::set_pinned(
                p,
                ProjectScope::Profile,
                target_path,
                pinned,
            ));
        }
        let mut updated_any = false;
        let mut hard_err: Option<projects::RegistryError> = None;
        for res in updates {
            match res {
                Ok(_) => updated_any = true,
                Err(projects::RegistryError::NotFound(_)) => {}
                Err(e) => hard_err = Some(e),
            }
        }
        match (hard_err, updated_any) {
            (Some(e), _) => Err(e),
            (None, true) => Ok(()),
            (None, false) => Err(projects::RegistryError::NotFound(format!(
                "No project for path '{}' found in any loaded scope",
                target_path
            ))),
        }
    }

    /// A trashed/archived row's agent was stopped deliberately, so refuse a start
    /// visibly and point at the restore key instead of swallowing the press.
    pub(in crate::tui) fn refuse_start_if_shelved(&mut self, id: &str) -> bool {
        let shelved = self.get_instance(id).and_then(|inst| {
            // A row mid-purge gets no restore hint: it would race the in-flight delete.
            if inst.status == Status::Deleting {
                return None;
            }
            match inst.ensure_startable() {
                Err(StartBlocked::Trashed) => Some(("Session in trash", "in the trash", "restore")),
                Err(StartBlocked::Archived) => Some(("Session archived", "archived", "unarchive")),
                Ok(()) => None,
            }
        });
        let Some((dialog_title, state, verb)) = shelved else {
            return false;
        };
        let key = if self.strict_hotkeys { "Z" } else { "z" };
        self.info_dialog = Some(InfoDialog::new(
            dialog_title,
            &format!(
                "This session is {state}; its agent stays stopped. Press {key} to {verb} it first."
            ),
        ));
        true
    }

    /// Restart the cursor's session, optionally migrating to a new profile and/or
    /// swapping the AI engine first.
    ///
    /// Guards: no selection and transient lifecycle (`Creating` / `Deleting`) drop;
    /// archived and trashed rows refuse with an info dialog pointing at the restore key;
    /// pane-dead rows drop silently; snoozed rows drop only under `Attention` sort, since
    /// elsewhere the snooze surface is hidden, so the flag is cleared and the restart
    /// runs. A repeat within 1.5s is debounced: overlapping cascades would each spawn a
    /// wake-up worker and tear down the still-booting pane.
    ///
    /// `new_profile` moves the session between profile storages and `new_tool` updates
    /// the field before respawn. A swap between two tool names running the same agent on
    /// different accounts carries the conversation across instead of parking it; see
    /// [`crate::session::conversation_carry::classify`]. The cascade runs on the
    /// `RestartPoller` (docker and the before_start hook block for seconds) and its
    /// `Instance` comes back through `apply_restart_results`. The wake-up message is
    /// `session.restart_wake_message`; empty disables it.
    pub(super) fn restart_selected_session(
        &mut self,
        new_profile: Option<&str>,
        new_tool: Option<&str>,
        new_extra_args: Option<&str>,
        new_command_override: Option<&str>,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let id = match &self.selected_session {
            Some(id) => id.clone(),
            None => return Ok(super::TransactionDisposition::Ignored),
        };

        // A cascade for this row is already on the worker. The 1.5s debounce below does
        // not cover a deliberate second press during a multi-second pull, and a duplicate
        // request would restart the row into the container the first one is building.
        if self.restart_in_flight.contains_key(&id) || self.transaction_is_pending(&id) {
            return Ok(super::TransactionDisposition::Ignored);
        }

        if self.refuse_start_if_shelved(&id) {
            return Ok(super::TransactionDisposition::Ignored);
        }

        // Skip transient rows. Snoozed rows only skip when the user is
        // in Attention sort; see method doc.
        let in_attention = self.sort_order == crate::session::config::SortOrder::Attention;
        let (skip, wake_snooze) = match self.get_instance(&id) {
            Some(inst) => {
                let snoozed = inst.is_snoozed();
                let skip = matches!(inst.status, Status::Creating | Status::Deleting)
                    || (snoozed && in_attention)
                    || inst.pane_dead_observed;
                let wake_snooze = snoozed && !in_attention;
                (skip, wake_snooze)
            }
            None => return Ok(super::TransactionDisposition::Ignored),
        };
        if skip {
            return Ok(super::TransactionDisposition::Ignored);
        }

        // Spam-debounce. Holding `e` or pressing it twice fast otherwise
        // races overlapping restart_with_size calls.
        let now = std::time::Instant::now();
        if let Some(prev) = self.restart_cooldown_at.get(&id) {
            if now.duration_since(*prev) < std::time::Duration::from_millis(1500) {
                return Ok(super::TransactionDisposition::Ignored);
            }
        }
        let row = self.capture_transaction_row(&id)?;
        let target = self.capture_transaction_target(new_profile, &row.before.source_profile)?;
        self.restart_cooldown_at.insert(id, now);
        self.request_transaction(persistence_transactions::TransactionRequest::Restart {
            row,
            target,
            tool: new_tool.map(str::to_owned),
            extra: new_extra_args.map(str::to_owned),
            command: new_command_override.map(str::to_owned),
            wake_snooze,
            size: crate::terminal::get_size(),
        })
    }

    pub(super) fn delete_selected(&mut self, options: &DeleteOptions) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            // Deleting a row mid-restart would fire docker commands against the
            // container the restart worker is creating and orphan resources.
            if self.restart_in_flight.contains_key(&id) {
                self.info_dialog = Some(InfoDialog::new(
                    "Restart in progress",
                    "This session is still restarting. Wait for it to finish before deleting.",
                ));
                return Ok(());
            }

            self.set_instance_status(&id, Status::Deleting);

            if let Some(inst) = self.get_instance(&id) {
                let request = DeletionRequest {
                    session_id: id.clone(),
                    instance: inst.clone(),
                    delete_worktree: options.delete_worktree,
                    delete_branch: options.delete_branch,
                    delete_sandbox: options.delete_sandbox,
                    force_delete: options.force_delete,
                    detach_hooks: true,
                    keep_scratch: options.keep_scratch,
                };
                self.request_deletion(request);
            }
        }
        Ok(())
    }

    pub(super) fn delete_selected_group(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(path) = self.selected_group.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let names = self
            .selected_group_profile
            .clone()
            .map(|p| vec![p])
            .unwrap_or_else(|| self.group_trees.keys().cloned().collect());
        let profiles = names
            .into_iter()
            .map(|p| {
                self.storages
                    .get(&p)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Original group profile unavailable: {p}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.request_transaction(persistence_transactions::TransactionRequest::DeleteGroup {
            profiles,
            path,
            restarting: self.restart_in_flight.keys().cloned().collect(),
            options: None,
        })
    }

    pub(super) fn delete_group_with_sessions(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(path) = self.selected_group.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let names = self
            .selected_group_profile
            .clone()
            .map(|p| vec![p])
            .unwrap_or_else(|| self.group_trees.keys().cloned().collect());
        let profiles = names
            .into_iter()
            .map(|p| {
                self.storages
                    .get(&p)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Original group profile unavailable: {p}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.request_transaction(persistence_transactions::TransactionRequest::DeleteGroup {
            profiles,
            path,
            restarting: self.restart_in_flight.keys().cloned().collect(),
            options: Some(persistence_transactions::GroupDeleteOptionsOwned {
                worktrees: options.delete_worktrees,
                branches: options.delete_branches,
                containers: options.delete_containers,
                force_worktrees: options.force_delete_worktrees,
            }),
        })
    }

    pub(super) fn prepare_force_removal(
        &self,
        instance: &Instance,
    ) -> anyhow::Result<super::PendingForceRemoval> {
        if let Some((request_id, pending)) = self
            .deletes_in_flight
            .iter()
            .find(|(_, pending)| pending.session_id == instance.id)
        {
            anyhow::ensure!(
                pending.matches(instance),
                "The original deletion no longer matches this incarnation"
            );
            return Ok(super::PendingForceRemoval::Existing {
                request_id: *request_id,
                control: pending.control.clone(),
            });
        }
        let (owner, control) = crate::session::deletion::PurgeOwner::issue(instance)?;
        Ok(super::PendingForceRemoval::Standalone {
            instance: Box::new(instance.clone()),
            owner,
            control,
        })
    }

    pub(super) fn force_remove_session(
        &mut self,
        target: super::PendingForceRemoval,
    ) -> anyhow::Result<()> {
        use crate::session::deletion::ForceIntent;
        match target {
            super::PendingForceRemoval::Existing {
                request_id,
                control,
            } => {
                let pending = self
                    .deletes_in_flight
                    .get_mut(&request_id)
                    .ok_or_else(|| anyhow::anyhow!("The original deletion is no longer pending"))?;
                let current = self
                    .instances
                    .get(&pending.session_id)
                    .ok_or_else(|| anyhow::anyhow!("The original session is no longer visible"))?;
                anyhow::ensure!(
                    pending.matches(current),
                    "The original deletion no longer matches this incarnation"
                );
                match control.request_force() {
                    ForceIntent::Accepted | ForceIntent::AlreadyRequested => pending.attempt.forced = true,
                    ForceIntent::TooLate => anyhow::bail!("The original deletion has already started hooks or commit; Force cannot change its cleanup"),
                    ForceIntent::Closed => anyhow::bail!("The original deletion has finished; Force cannot retarget another owner"),
                }
            }
            super::PendingForceRemoval::Standalone {
                instance,
                owner,
                control,
            } => {
                anyhow::ensure!(
                    !self.has_delete_in_flight(&instance.id),
                    "A deletion started after confirmation; retry against its original owner"
                );
                let current = self
                    .instances
                    .get(&instance.id)
                    .ok_or_else(|| anyhow::anyhow!("The original session is no longer visible"))?;
                anyhow::ensure!(
                    control.matches(current),
                    "The confirmed original session changed before Force removal"
                );
                let pending =
                    super::PendingDeletion::from_control(&instance, true, control.clone())?;
                anyhow::ensure!(
                    control.request_force() == ForceIntent::Accepted,
                    "The confirmed force owner is no longer available"
                );
                self.set_instance_status(&instance.id, Status::Deleting);
                let request_id = self.deletion_poller.request_force_remove(*instance, owner);
                self.deletes_in_flight.insert(request_id, pending);
            }
        }
        Ok(())
    }

    pub(super) fn request_deletion(&mut self, request: DeletionRequest) {
        let (pending, owner) =
            match super::PendingDeletion::capture(&request.instance, request.force_delete) {
                Ok(pending) => pending,
                Err(error) => {
                    self.info_dialog =
                        Some(InfoDialog::new("Delete refused", &format!("{error:#}")));
                    return;
                }
            };
        let request_id = self.deletion_poller.request_deletion(request, owner);
        self.deletes_in_flight.insert(request_id, pending);
    }

    fn has_delete_in_flight(&self, session_id: &str) -> bool {
        self.deletes_in_flight
            .values()
            .any(|pending| pending.session_id == session_id)
    }

    /// Whether a trashed row's last delete was forced, when it failed in the row's current
    /// trash lifecycle and no other delete for it is in flight.
    pub(super) fn failed_delete_forced(&self, inst: &Instance) -> Option<bool> {
        if self.has_delete_in_flight(&inst.id) {
            return None;
        }
        self.failed_deletes
            .get(&inst.id)
            .filter(|attempt| inst.trashed_at.is_some() && attempt.trashed_at == inst.trashed_at)
            .map(|attempt| attempt.forced)
    }

    /// Retain filesystem artifacts while the canonical Purge worker settles the original.
    fn drop_failed_trashed_session(&mut self, inst: &Instance) {
        let (pending, owner) = match super::PendingDeletion::capture(inst, true) {
            Ok(pending) => pending,
            Err(error) => {
                self.info_dialog = Some(InfoDialog::new("Delete refused", &format!("{error:#}")));
                return;
            }
        };
        self.set_instance_status(&inst.id, Status::Deleting);
        let request_id = self.deletion_poller.request_drop(inst.clone(), owner);
        self.deletes_in_flight.insert(request_id, pending);
    }

    pub(super) fn group_has_managed_worktrees(
        &self,
        group_path: &str,
        prefix: &str,
        owning_profile: Option<&str>,
    ) -> bool {
        let is_member = group_membership(group_path, prefix, owning_profile);
        self.instances()
            .any(|i| is_member(i) && i.has_managed_worktree_or_workspace())
    }

    pub(super) fn group_has_containers(
        &self,
        group_path: &str,
        prefix: &str,
        owning_profile: Option<&str>,
    ) -> bool {
        let is_member = group_membership(group_path, prefix, owning_profile);
        self.instances()
            .any(|i| is_member(i) && i.sandbox_info.as_ref().is_some_and(|s| s.enabled))
    }

    /// Rename a group in-place: the old group path is removed and all sessions and
    /// sub-groups follow the new name. Re-sorting happens automatically on reload.
    pub(super) fn rename_selected_group(
        &mut self,
        new_group: Option<&str>,
        new_profile: Option<&str>,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(ctx) = self.group_rename_context.take() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let new_path = new_group
            .filter(|g| !g.is_empty())
            .unwrap_or(&ctx.old_path)
            .to_owned();
        if new_path == ctx.old_path && new_profile.is_none_or(|p| p == ctx.old_profile) {
            return Ok(super::TransactionDisposition::Ignored);
        }
        let source = self
            .storages
            .get(&ctx.old_profile)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Original group profile unavailable"))?;
        let target = self.capture_transaction_target(new_profile, &ctx.old_profile)?;
        let prefix = format!("{}/", ctx.old_path);
        let members = self
            .instances
            .values()
            .filter(|r| {
                r.source_profile == ctx.old_profile
                    && (r.group_path == ctx.old_path || r.group_path.starts_with(&prefix))
            })
            .cloned()
            .map(persistence_transactions::RowCapture::capture)
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.request_transaction(persistence_transactions::TransactionRequest::Group {
            source,
            target,
            old_path: ctx.old_path,
            new_path,
            members,
        })
    }

    pub(super) fn submit_runner_settlement(
        &mut self,
        origin: super::RequestOrigin,
        request: crate::tui::stop_poller::SettlementRequest,
    ) {
        self.settlement_in_flight
            .insert(request.session_id.clone(), origin);
        self.settlement_poller.request(request);
    }

    /// Edit the selected session's worktree workdir name: move the worktree directory
    /// and, optionally, rename its git branch, persisting both. See #1723.
    pub(super) fn set_worktree_name_for_selected(
        &mut self,
        new_name: &str,
        rename_branch: bool,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        self.set_worktree_name_by_id(&id, new_name, rename_branch)
    }

    pub(super) fn set_worktree_name_by_id(
        &mut self,
        id: &str,
        new_name: &str,
        rename_branch: bool,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let row = self.capture_transaction_row(id)?;
        let settled = self.settled_edit.take();
        self.request_transaction(persistence_transactions::TransactionRequest::Workdir {
            row,
            name: new_name.to_owned(),
            rename_branch,
            settled,
        })
    }

    /// Attach a repo to `id` and, when a worker is live, restart it so the agent can see
    /// the new root (#3103). The worktree is created before anything is persisted, so a
    /// save failure rolls it back rather than leaving an orphan. The restart goes through
    /// the marker `aoe acp restart` writes, so the daemon respawns with the stored ACP
    /// session id and the transcript survives.
    ///
    /// Returns as soon as the request is queued; the outcome arrives through
    /// [`super::HomeView::apply_attach_project_results`]. Every check that can be made
    /// from the in-memory instance is made here rather than on the worker, so an `Err` is
    /// a refusal the caller can show immediately.
    pub(super) fn add_project_to_session(
        &mut self,
        id: &str,
        repo_path: &std::path::Path,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(instance) = self.get_instance(id).cloned() else {
            anyhow::bail!("Session no longer exists");
        };
        // Defence in depth behind the picker's own gate: this is the choke point both
        // TUI entry points share, and what SIGTERMs the worker below.
        if matches!(
            instance.status,
            crate::session::Status::Creating | crate::session::Status::Deleting
        ) {
            anyhow::bail!(
                "Wait for the session to finish starting or deleting before attaching a project"
            );
        }
        // The same set the picker refuses: `Waiting` and `Starting` are turns in flight
        // too, and killing the worker in `Waiting` discards a pending approval.
        if instance.status.blocks_worktree_edit() {
            anyhow::bail!(
                "The agent is mid-turn and attaching restarts it; wait for the turn to finish or stop the session first"
            );
        }
        // Trashed and archived too, so a status flip while the picker is open cannot
        // slip an attach onto a deliberately stopped agent.
        if instance.is_trashed() {
            anyhow::bail!("This session is in the trash; restore it before attaching a project");
        }
        if instance.is_archived() {
            anyhow::bail!(
                "This session is archived and its agent stays stopped; unarchive it before attaching a project"
            );
        }
        // One attach per session at a time: a second would race the first one's
        // worktree creation and its worker bounce.
        if self.attach_project_in_flight.contains_key(id) || self.transaction_is_pending(id) {
            anyhow::bail!("An attach is already running for this session; wait for it to finish");
        }
        let row = self.capture_transaction_row(id)?;
        self.request_transaction(
            persistence_transactions::TransactionRequest::AttachProject {
                row,
                repo_path: repo_path.to_path_buf(),
            },
        )
    }

    pub(super) fn rename_selected(
        &mut self,
        new_title: &str,
        new_group: Option<&str>,
        new_profile: Option<&str>,
        rename_branch: bool,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        self.rename_session_by_id(&id, new_title, new_group, new_profile, rename_branch)
    }

    pub(super) fn rename_session_by_id(
        &mut self,
        id: &str,
        new_title: &str,
        new_group: Option<&str>,
        new_profile: Option<&str>,
        rename_branch: bool,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let row = self.capture_transaction_row(id)?;
        let target = self.capture_transaction_target(new_profile, &row.before.source_profile)?;
        let settled = self.settled_edit.take();
        self.request_transaction(persistence_transactions::TransactionRequest::Rename {
            row,
            target,
            title: new_title.to_owned(),
            group: new_group.map(str::to_owned),
            rename_branch,
            settled,
        })
    }

    /// Snooze keybind: an already-snoozed row wakes immediately, otherwise the duration
    /// picker opens and `snooze_session_for` runs on submit.
    ///
    /// A snooze is a temporary archive: `snoozed_until = now + minutes` sinks the row to
    /// tier 99, renders it italic+dim with a `z ` prefix and the remaining time, and it
    /// wakes lazily when the timer elapses. The duration is resolved at snooze time, so
    /// changing the config default does not extend one in flight.
    pub(super) fn toggle_snooze_at_cursor(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let (is_snoozed, title) = {
            let inst = self.instances.get(&id);
            match inst {
                Some(i) => (i.is_snoozed(), i.title.clone()),
                None => return Ok(super::TransactionDisposition::Ignored),
            }
        };
        if is_snoozed {
            return self.apply_user_action_after(
                &id,
                |inst| inst.unsnooze(),
                persistence_transactions::MetadataContinuation::Snooze {
                    id: id.clone(),
                    message: format!("Woke: {title}"),
                },
            );
        }

        self.pending_snooze_session = Some(id);
        self.snooze_duration_dialog = Some(crate::tui::dialogs::SnoozeDurationDialog::new(&title));
        Ok(super::TransactionDisposition::Ignored)
    }

    /// Apply a snooze with an explicit duration, on the picker's submit. The only place
    /// the TUI mutates `snoozed_until`. Jumps to the next needs-attention row once this
    /// one sinks, so triage can continue.
    pub(super) fn snooze_session_for(
        &mut self,
        id: &str,
        minutes: u32,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let title = self
            .instances
            .get(id)
            .map(|i| i.title.clone())
            .unwrap_or_default();
        self.apply_user_action_after(
            id,
            |inst| inst.snooze(minutes),
            persistence_transactions::MetadataContinuation::Snooze {
                id: id.to_owned(),
                message: format!("Snoozed for {}: {}", humanize_minutes(minutes), title),
            },
        )
    }

    /// Toggle the favorite flag on the cursor's session. Favorite survives an unsnooze
    /// but not an archive; that mutual exclusion lives in `Instance::archive()`.
    pub(super) fn toggle_favorite_at_cursor(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let is_fav = match self.instances.get(&id) {
            Some(i) => i.is_favorited(),
            None => return Ok(super::TransactionDisposition::Ignored),
        };
        self.apply_user_action_after(
            &id,
            |inst| {
                if is_fav {
                    inst.unfavorite()
                } else {
                    inst.favorite()
                }
            },
            persistence_transactions::MetadataContinuation::Reseat(id.clone()),
        )?;
        Ok(super::TransactionDisposition::Queued)
    }

    /// The session the cursor should land on once the cursor's row is archived: the
    /// nearest visible non-archived session below, else above. Rows inside collapsed
    /// groups and rows already under the Archived section are not candidates, so the
    /// cursor never jumps into either. `None` leaves the caller to clamp the index.
    pub(super) fn archive_successor_session(&self, archiving_id: &str) -> Option<String> {
        let candidate = |item: &Item| -> Option<String> {
            let Item::Session { id, .. } = item else {
                return None;
            };
            if id == archiving_id {
                return None;
            }
            let inst = self.instances.get(id)?;
            (!inst.is_archived() && !inst.is_trashed()).then(|| id.clone())
        };
        for item in self.flat_items.iter().skip(self.cursor + 1) {
            if let Some(id) = candidate(item) {
                return Some(id);
            }
        }
        for item in self.flat_items.iter().take(self.cursor).rev() {
            if let Some(id) = candidate(item) {
                return Some(id);
            }
        }
        None
    }

    /// Manual unread toggle (`U`), symmetric in both directions. The row's
    /// `theme.unread` color is the feedback, so there is no toast. No-op when disabled.
    pub(super) fn toggle_unread_at_cursor(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        if !crate::session::unread_enabled() {
            return Ok(super::TransactionDisposition::Ignored);
        }
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        if !self.instances.contains_key(&id) {
            return Ok(super::TransactionDisposition::Ignored);
        }
        self.apply_user_action_after(
            &id,
            |inst| inst.toggle_unread(),
            persistence_transactions::MetadataContinuation::Unread(id.clone()),
        )?;
        Ok(super::TransactionDisposition::Queued)
    }

    /// Toggle archive on the cursor's session. Archive tears down every tmux session
    /// (agent plus ancillary) but keeps worktree, branch and container. Unarchive does
    /// not respawn: press `e`, or send a message to auto-unarchive. See #1868.
    pub(super) fn toggle_archive_at_cursor(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        self.toggle_archive_by_id(&id)
    }

    pub(super) fn toggle_archive_by_id(
        &mut self,
        id: &str,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let row = self.capture_transaction_row(id)?;
        if self.settled_edit.is_none() && row.before.is_trashed() {
            self.restore_selected_from_trash();
            return Ok(super::TransactionDisposition::Queued);
        }
        if self.settled_edit.is_none() && row.before.is_archived() {
            return self.apply_user_action(id, |r| r.unarchive());
        }
        let settled = self.settled_edit.take();
        let successor = self.archive_successor_session(id);
        self.request_transaction(persistence_transactions::TransactionRequest::Archive {
            row,
            settled,
            reveal: false,
            successor,
        })
    }

    /// Move a session to the trash and set `trashed_at`, keeping durable artifacts so it
    /// can be restored. The Trash section's collapse state is left untouched; its header
    /// count is the feedback (#2489).
    ///
    /// The durable trash marker is written inline so the row flips immediately.
    /// Everything that can block, tmux teardown, the container stop and the worktree
    /// relocation, runs on the `TrashPoller` and is reconciled by
    /// [`apply_trash_results`](crate::tui::home::HomeView::apply_trash_results): a live
    /// bind mount makes the worktree move fail EBUSY, but `docker stop` blocks for the
    /// container's grace period, which froze the input thread inline (#1496).
    /// The trash worker settles the durable runner journal before teardown.
    pub(super) fn trash_session_by_id(&mut self, id: &str) {
        let result = self.capture_transaction_row(id).and_then(|row| {
            self.request_transaction(persistence_transactions::TransactionRequest::Trash(row))
        });
        if let Err(error) = result {
            self.info_dialog = Some(InfoDialog::new("Trash Failed", &format!("{error:#}")));
        }
    }

    /// Restore the selected trashed session, clearing `trashed_at` so it returns to its
    /// prior bucket. No-op when the selection is not trashed; the session stays stopped
    /// until the user restarts it with `e`. See #2489.
    pub(super) fn restore_selected_from_trash(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(row) = self.get_instance(&id).filter(|r| r.is_trashed()) else {
            return;
        };
        let owned_generation = self.trash_poller.owned_generation(&row.source_profile, &id);
        let result = self.capture_transaction_row(&id).and_then(|row| {
            self.request_transaction(persistence_transactions::TransactionRequest::Restore {
                row,
                owned_generation,
            })
        });
        if let Err(error) = result {
            self.info_dialog = Some(InfoDialog::new("Restore Failed", &format!("{error:#}")));
        }
    }

    /// Restore every trashed session, driving each row through the same
    /// `restore_selected_from_trash` as a single restore so the claim/commit races
    /// (#2541) are handled identically. Each failure surfaces its own info dialog; the
    /// last one wins, which is acceptable for a rare bulk recovery.
    pub(super) fn restore_all_from_trash(&mut self) {
        let ids: Vec<String> = self
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .map(|i| i.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            // `restore_selected_from_trash` acts on the selection, so point it at each
            // row in turn; it re-selects the restored session, which the next iteration
            // overwrites.
            self.selected_session = Some(id);
            self.restore_selected_from_trash();
        }
    }

    /// Unarchive every archived session. Rows stay Stopped, same as a single unarchive.
    /// Reversible, so no confirmation upstream.
    pub(super) fn unarchive_all(&mut self) {
        let ids: Vec<String> = self
            .instances
            .values()
            .filter(|i| i.is_archived() && !i.is_trashed())
            .map(|i| i.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        if let Err(e) = self.bulk_apply_user_action(&ids, |inst| inst.unarchive()) {
            tracing::error!(target: "tui.home", "unarchive_all failed: {e}");
        }
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    /// Permanently purge every trashed session, reached only after the confirm dialog.
    /// Each row runs the same off-thread deletion path as a single permanent delete, with
    /// cleanup options resolved per row from its repo config (mirroring the CLI
    /// `empty-trash`). A row whose last delete failed is forced when `force_failed`, and
    /// one whose forced delete failed is removed from aoe without cleanup when
    /// `drop_failed`; otherwise each retries at its previous level.
    pub(super) fn empty_trash_all(&mut self, force_failed: bool, drop_failed: bool) {
        let mut trashed: Vec<Instance> = self
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .cloned()
            .collect();
        trashed.sort_by(|left, right| left.id.cmp(&right.id));
        if trashed.is_empty() {
            return;
        }
        for inst in trashed {
            let id = inst.id.clone();
            // Do not race teardown with a restart or another pending delete.
            if self.restart_in_flight.contains_key(&id) || self.has_delete_in_flight(&id) {
                continue;
            }
            let force_delete = match self.failed_delete_forced(&inst) {
                None => false,
                Some(false) => force_failed,
                Some(true) if drop_failed => {
                    self.drop_failed_trashed_session(&inst);
                    continue;
                }
                Some(true) => true,
            };

            self.set_instance_status(&id, Status::Deleting);

            let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                &inst.source_profile,
                std::path::Path::new(&inst.project_path),
            );
            let delete_worktree =
                config.worktree.auto_cleanup && inst.has_managed_worktree_or_workspace();
            let delete_branch = delete_worktree && config.worktree.delete_branch_on_cleanup;
            let delete_sandbox = inst.sandbox_info.as_ref().is_some_and(|s| s.enabled)
                && config.sandbox.auto_cleanup;

            self.request_deletion(DeletionRequest {
                session_id: id.clone(),
                instance: inst.clone(),
                delete_worktree,
                delete_branch,
                delete_sandbox,
                force_delete,
                detach_hooks: true,
                keep_scratch: false,
            });
        }
        // Rows show Deleting until the poller reports each transaction.
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    /// The active (non-archived) session ids under the selected group header, honoring
    /// the group-by mode. Archived rows already live under the Archived section, so they
    /// are excluded. Empty when no group is selected.
    pub(super) fn active_sessions_in_selected_group(&self) -> Vec<String> {
        let Some(group_path) = self.selected_group.as_deref() else {
            return Vec::new();
        };
        match self.group_by {
            // Project headers are derived from each session's repo name and unified
            // across profiles, narrowed only by the active profile filter, exactly as
            // `build_flat_items_by_project` builds them.
            crate::session::config::GroupByMode::Project => self
                .instances
                .values()
                .filter(|i| !i.is_archived() && !i.is_trashed())
                .filter(|i| {
                    self.active_profile
                        .as_ref()
                        .is_none_or(|p| &i.source_profile == p)
                })
                .filter(|i| super::project_group_key(i) == group_path)
                .map(|i| i.id.clone())
                .collect(),
            // Org headers key on the host-scoped owner so same-named owners on
            // different hosts stay separate; same cross-profile unification as Project.
            crate::session::config::GroupByMode::Org => self
                .instances
                .values()
                .filter(|i| !i.is_archived() && !i.is_trashed())
                .filter(|i| {
                    self.active_profile
                        .as_ref()
                        .is_none_or(|p| &i.source_profile == p)
                })
                .filter(|i| self.org_group_key(i) == group_path)
                .map(|i| i.id.clone())
                .collect(),
            // Manual groups nest, so a session belongs when its path matches exactly or
            // sits beneath the group; scoped to the owning profile the same way
            // `delete_selected_group` does.
            crate::session::config::GroupByMode::Manual => {
                let prefix = format!("{}/", group_path);
                let is_member =
                    group_membership(group_path, &prefix, self.selected_group_profile.as_deref());
                self.instances
                    .values()
                    .filter(|i| !i.is_archived() && !i.is_trashed())
                    .filter(|i| is_member(i))
                    .map(|i| i.id.clone())
                    .collect()
            }
        }
    }

    /// Queue each original group member for reversible, proven-quiescent archive.
    pub(super) fn archive_selected_group(
        &mut self,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let mut ids = self.active_sessions_in_selected_group();
        ids.sort();
        let rows = ids
            .iter()
            .map(|id| self.capture_transaction_row(id))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok(super::TransactionDisposition::Ignored);
        }
        for row in rows {
            let successor = self.archive_successor_session(&row.before.id);
            self.request_transaction(persistence_transactions::TransactionRequest::Archive {
                row,
                settled: None,
                reveal: true,
                successor,
            })?;
        }
        Ok(super::TransactionDisposition::Queued)
    }
}

/// Outcome of a TUI restore-from-trash driven directly against storage. See #2541.
pub(super) enum RestoreFromTrash {
    Restored,
    AlreadyGone,
    Busy(String),
    WorktreeFailed { reason: String },
    PersistFailed,
}

/// Restore under workspace -> identity -> lifecycle locks.
pub(super) fn restore_from_trash_with_storage(
    storage: &Storage,
    id: &str,
    owned_trash_generation: Option<u64>,
    before: &Instance,
) -> RestoreFromTrash {
    let _workspace_claim_lock = match acquire_session_workspace_claim_lock() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore workspace claim lock failed: {error}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    let _identity_lock = match acquire_session_identity_lock() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore identity lock failed: {error}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    let storage = match storage.reopen_preserving_watch() {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore profile open failed: {error}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(id) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore lock failed: {error}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    let decision = match storage.update_under_workspace_claim_lock(|instances, _groups| {
        let original = instances
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("Original restore row disappeared"))?;
        anyhow::ensure!(
            original.created_at == before.created_at
                && original.same_storage_origin(before)
                && (original.lifecycle_generation == before.lifecycle_generation
                    || owned_trash_generation == Some(original.lifecycle_generation)),
            "Original restore row changed"
        );
        let decision = match owned_trash_generation {
            Some(generation) => crate::session::claim::decide_restore_claim_after_trash(
                instances,
                id,
                generation,
                chrono::Utc::now(),
            ),
            None => crate::session::claim::decide_restore_claim(instances, id, chrono::Utc::now()),
        };
        decision.map_err(anyhow::Error::new)
    }) {
        Ok(decision) => decision,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore reservation failed: {error}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    let generation = match decision {
        crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
        crate::session::claim::RestoreClaimDecision::AlreadyGone => {
            return RestoreFromTrash::AlreadyGone;
        }
        crate::session::claim::RestoreClaimDecision::Busy(holder) => {
            return RestoreFromTrash::Busy(holder.busy_reason());
        }
    };

    let loaded = match storage.load_strict_for_worktree_ownership_locked() {
        Ok(all) => all.into_iter().find(|instance| instance.id == id),
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore load failed: {error}");
            let _ = storage.update_under_workspace_claim_lock(|instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    );
                }
                Ok(())
            });
            return RestoreFromTrash::PersistFailed;
        }
    };
    let Some(snapshot) = loaded else {
        return RestoreFromTrash::AlreadyGone;
    };
    if !snapshot.lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation) {
        return RestoreFromTrash::Busy(crate::session::NEWER_GENERATION_BUSY_REASON.to_string());
    }
    let needs_move = snapshot
        .pre_trash_project_path
        .as_ref()
        .is_some_and(|original| original != &snapshot.project_path);
    // Inventory reads precede the profile storage flock.
    if needs_move {
        let paths = [
            std::path::PathBuf::from(&snapshot.project_path),
            std::path::PathBuf::from(snapshot.pre_trash_project_path.as_ref().unwrap()),
        ];
        if let Err(error) = crate::session::deletion::ensure_unclaimed_paths(
            crate::session::deletion::SessionPathOwner {
                profile: storage.profile(),
                session_id: id,
            },
            &paths,
        ) {
            let _ = storage.update_under_workspace_claim_lock(|instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|row| row.id == id) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    );
                }
                Ok(())
            });
            return RestoreFromTrash::WorktreeFailed {
                reason: format!("worktree ownership could not be verified: {error}"),
            };
        }
    }
    let result = storage.update_under_workspace_claim_lock(|instances, _groups| {
        let Some(stored) = instances.iter_mut().find(|row| row.id == id) else {
            return Ok(RestoreFromTrash::AlreadyGone);
        };
        if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation)
            || !restore_plan_unchanged(&snapshot, stored)
        {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
            return Ok(RestoreFromTrash::Busy(
                crate::session::NEWER_GENERATION_BUSY_REASON.to_string(),
            ));
        }
        let mut instance = stored.clone();
        instance.source_profile = storage.profile().to_owned();
        if let crate::session::trash::RestoreOutcome::Failed { reason } =
            crate::session::trash::restore_worktree_location(&mut instance)
        {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
            return Ok(RestoreFromTrash::WorktreeFailed { reason });
        }
        let restored_path = instance.project_path;
        let restored_pre = instance.pre_trash_project_path;
        anyhow::ensure!(
            crate::session::claim::finalize_restore_commit(
                instances,
                id,
                generation,
                &restored_path,
                &restored_pre,
            ) == crate::session::claim::RestoreCommit::Committed,
            "restore reservation was superseded during worktree move",
        );
        Ok(RestoreFromTrash::Restored)
    });
    match result {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore commit failed: {error}");
            let _ = storage.update_under_workspace_claim_lock(|instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|row| row.id == id) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    );
                }
                Ok(())
            });
            RestoreFromTrash::PersistFailed
        }
    }
}

fn restore_plan_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    snapshot.is_trashed() == durable.is_trashed()
        && snapshot.project_path == durable.project_path
        && snapshot.pre_trash_project_path == durable.pre_trash_project_path
        && snapshot.worktree_info == durable.worktree_info
        && snapshot.scratch == durable.scratch
        && snapshot
            .workspace_info
            .as_ref()
            .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
            == durable
                .workspace_info
                .as_ref()
                .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
        && snapshot.is_sandboxed() == durable.is_sandboxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    // An Idle sandbox session whose container is still running is the #1927 follow-up:
    // the worktree dir is an active bind-mount source, so `git worktree move` fails with
    // EBUSY and the rename must block with the sandbox-specific reason.
    #[test]
    fn idle_sandbox_with_running_container_blocks() {
        assert_eq!(
            worktree_rename_block(Status::Idle, true, true),
            Some(WorktreeRenameBlock::SandboxContainer)
        );
    }

    #[test]
    fn idle_sandbox_with_stopped_container_is_safe() {
        // Stopping the session tears the container down, releasing the mount.
        assert_eq!(worktree_rename_block(Status::Idle, true, false), None);
    }

    #[test]
    fn worktree_rename_block_checks_status_before_container() {
        // (status, sandboxed, container running, expected block)
        let mut cases = vec![
            // No container, nothing holds the dir; the move proceeds.
            (Status::Idle, false, false, None),
            // A busy agent reports as ActiveAgent even with a live container.
            (
                Status::Running,
                true,
                true,
                Some(WorktreeRenameBlock::ActiveAgent),
            ),
        ];
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
            Status::Deleting,
        ] {
            cases.push((status, false, false, Some(WorktreeRenameBlock::ActiveAgent)));
        }
        for (status, sandboxed, running, want) in cases {
            assert_eq!(
                worktree_rename_block(status, sandboxed, running),
                want,
                "{status:?} sandboxed={sandboxed} running={running}"
            );
        }
    }
}

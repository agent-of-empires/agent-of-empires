//! Session operations for HomeView (create, delete, rename)

use crate::session::{
    acquire_session_identity_lock, duplicate_session_error, is_duplicate_session, list_profiles,
    GroupMovePlan, Instance, Item, LifecycleOperation, Status, Storage,
};
use crate::tui::deletion_poller::DeletionRequest;
use crate::tui::dialogs::{DeleteOptions, GroupDeleteOptions, InfoDialog};

use super::{HomeView, PendingArchiveCursor};

/// Membership predicate for a manual group: matches instances whose
/// `group_path` equals `group_path` or nests beneath it, optionally scoped to
/// a single owning profile (`None` matches every profile). `prefix` must be
/// `"{group_path}/"`; it is taken as an argument rather than computed here
/// because `group_has_managed_worktrees` / `group_has_containers` already
/// receive it precomputed from their call sites.
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

enum PersistGroupDelete {
    Ready(Vec<Instance>),
    Creating,
    Restarting,
}

fn rekey_tmux_after_persist(id: &str, old_title: &str, new_title: &str) -> Option<String> {
    if old_title == new_title {
        return None;
    }
    match crate::tmux::rekey_session(id, old_title, new_title) {
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(target: "tui.home", session = %id, "tmux rename failed after persistence: {error}");
            Some(format!(
                "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
            ))
        }
    }
}

/// Why a tied-worktree rename must refuse to move the worktree directory.
///
/// `git worktree move` does a `rename(2)` on the worktree dir, which the
/// kernel refuses while anything holds it. Two distinct holders matter and
/// they need different wording: an active agent (the session's `status`),
/// and a sandbox session's container, which bind-mounts the worktree dir and
/// stays alive on `sleep infinity` even while the agent is Idle. Both are
/// cleared by stopping the session.
#[derive(Debug, PartialEq, Eq)]
enum WorktreeRenameBlock {
    /// The session's agent is busy (running, starting, etc.).
    ActiveAgent,
    /// A sandbox container is running and mounting the worktree dir.
    SandboxContainer,
}

/// Decide whether a tied-worktree rename must be blocked, and why. Status
/// takes precedence so a busy agent reports as `ActiveAgent` rather than
/// reaching for the container reason. Returns `None` when the move is safe.
fn worktree_rename_block(
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

fn worktree_rename_block_message(reason: &WorktreeRenameBlock) -> &'static str {
    match reason {
        WorktreeRenameBlock::ActiveAgent => "This worktree session's directory moves to match the new name, which can't happen while it's running. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
        WorktreeRenameBlock::SandboxContainer => "This sandbox session's container is mounting the worktree directory, so it can't be moved to match the new name. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
    }
}

impl HomeView {
    /// Pin or unpin the project header under the cursor (project view only).
    ///
    /// Pinning keeps the repo's header in project view even after its last
    /// session is gone: it registers the repo if needed (the same global
    /// registry the WebUI writes) and sets its `pinned` flag. Unpinning clears
    /// the flag but KEEPS the registry entry, so the project stays a saved
    /// project (still in the Projects view and the new-session wizard); its
    /// header just drops once it has no sessions. Only an explicit remove (the
    /// projects dialog) deletes the entry. See #2208.
    ///
    /// The registry is the shared persistence layer, so this goes through the
    /// same `projects::add` / `projects::set_pinned` the web API and the
    /// projects dialog use; canonicalization and conflict rules stay in one
    /// place.
    pub(super) fn toggle_project_pin_at_cursor(&mut self) {
        use crate::session::{projects, Project, ProjectScope};
        use crate::tui::dialogs::InfoDialog;

        let Some(label) = self.project_group_at_cursor() else {
            return;
        };
        let profile = self.config_profile();
        // The header's own repo path (canonical), or None for an empty pinned
        // header. Keying on the path keeps two repos that share a basename
        // independent, so the toggle acts on the repo the user is looking at.
        let header_path = self.project_header_repo_path(&label);

        if self.is_project_label_pinned(&label) {
            // Unpin. Prefer the registry entry whose canonical path matches the
            // header's own repo. An empty header has no session path, so fall
            // back to the basename match (it exists only because a pinned
            // project carries that basename; two such empties share one header
            // and clear one per press).
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
            // Pin the repo backing this header. An unpinned header always has at
            // least one live session (an empty header is pinned by
            // construction), so its repo path is known. If the repo is already
            // saved (registered but not pinned), flip its flag; otherwise
            // register it pinned.
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

    /// Set the `pinned` flag on every registry entry for `target_path`'s
    /// canonical path, across the global file and every loaded profile (plus
    /// the default profile). A path can be registered in more than one scope at
    /// once (`--allow-override` lets a profile entry shadow a global one), and
    /// `registered_projects` drops which profile each entry came from in
    /// all-profiles mode, so a single visible entry is not enough. `NotFound`
    /// per scope is ignored; a real I/O/parse failure is surfaced even if
    /// another scope updated, since a partial toggle the user can't see is
    /// worse than a visible error; no match anywhere is `NotFound`. See #2208.
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
        let mut updates = vec![projects::update(
            profile,
            ProjectScope::Global,
            target_path,
            projects::ProjectPatch {
                pinned: Some(pinned),
                ..Default::default()
            },
        )
        .map(|_| ())];
        for p in &profiles {
            updates.push(
                projects::update(
                    p,
                    ProjectScope::Profile,
                    target_path,
                    projects::ProjectPatch {
                        pinned: Some(pinned),
                        ..Default::default()
                    },
                )
                .map(|_| ()),
            );
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
    /// Restart the cursor's session, optionally migrating to a new profile
    /// and/or swapping the AI engine first.
    ///
    /// Guards (apply to bare `e` / `E` / `F5` and dialog-submitted restarts):
    /// - No selection: no-op.
    /// - Transient lifecycle (`Creating` / `Deleting`): drop.
    /// - Sunk rows: archived and trashed rows refuse with an info dialog
    ///   pointing at the restore key (archive's contract is "do not
    ///   auto-revive", but a silent drop read as a swallowed failure);
    ///   pane-dead rows still drop silently (they have a dedicated revive
    ///   path). Snoozed rows drop only when `sort_order == Attention`; in other
    ///   sort modes the snooze surface is hidden, so silently swallowing
    ///   the press would leave the user staring at a row that looks
    ///   restartable but isn't. Outside Attention we clear the snooze flag
    ///   and let the restart proceed so behavior matches what the user
    ///   sees on screen.
    /// - Spam-debounce: if the same session was restarted within the last
    ///   1.5s, the press is dropped. Without this guard rapid `e` presses
    ///   would each submit a daemon start and churn the still-booting agent
    ///   via overlapping starts.
    ///
    /// - `new_profile`: when `Some(p)` and `p` differs from the current
    ///   `source_profile`, the session moves between profile storages.
    ///   Mirrors the profile-move path in `rename_selected` so a restart-
    ///   with-different-profile behaves the same as rename + restart.
    /// - `new_tool`: when `Some(t)` and `t` differs from the current `tool`,
    ///   the field is updated before respawn so the new agent binary starts
    ///   on the next launch.
    ///
    /// Launch edits travel with the restart command. Only the daemon may commit
    /// them after lifecycle admission; a disconnected or refused request leaves
    /// both this view and the stored session untouched.
    pub(super) fn restart_selected_session(
        &mut self,
        new_profile: Option<&str>,
        new_tool: Option<&str>,
        new_extra_args: Option<&str>,
        new_command_override: Option<&str>,
    ) -> anyhow::Result<()> {
        let id = match &self.selected_session {
            Some(id) => id.clone(),
            None => return Ok(()),
        };

        // A daemon start for this row is already in flight. The start runs
        // off the event loop now, so the 1.5s keyboard-repeat debounce below
        // does not cover a deliberate second press during a multi-second
        // start. Without this guard a duplicate submit would churn the
        // still-booting agent a second time.
        if self.restart_in_flight.contains(&id) {
            return Ok(());
        }

        // A trashed/archived row's refusal must be visible: its agent was
        // deliberately stopped, so a silent no-op here read as a swallowed
        // failure (the row just sits there). Point at the restore key instead.
        let shelved = self.get_instance(&id).and_then(|inst| {
            // A row mid-purge gets no restore/unarchive hint: same rationale
            // as `render_shelf_deleting_preview`, which drops those hints so
            // they don't race the in-flight delete. Falls through to the
            // transient skip below, which drops Deleting silently.
            if inst.status == Status::Deleting {
                None
            } else if inst.is_trashed() {
                Some(("Session in trash", "in the trash", "restore"))
            } else if inst.is_archived() {
                Some(("Session archived", "archived", "unarchive"))
            } else {
                None
            }
        });
        if let Some((dialog_title, state, verb)) = shelved {
            let key = if self.strict_hotkeys { "Z" } else { "z" };
            self.info_dialog = Some(InfoDialog::new(
                dialog_title,
                &format!("This session is {state}; its agent stays stopped. Press {key} to {verb} it first."),
            ));
            return Ok(());
        }

        // Skip transient rows. Snoozed rows only skip when the user is
        // in Attention sort; see method doc.
        let in_attention = self.sort_order == crate::session::config::SortOrder::Attention;
        let (skip, wake_snooze) = match self.get_instance(&id) {
            Some(inst) => {
                let snoozed = inst.is_snoozed();
                let skip = matches!(inst.status, Status::Creating | Status::Deleting)
                    || (snoozed && in_attention);
                let wake_snooze = snoozed && !in_attention;
                (skip, wake_snooze)
            }
            None => return Ok(()),
        };
        if skip {
            return Ok(());
        }

        // Spam-debounce. Holding `e` or pressing it twice fast otherwise
        // races overlapping restart_with_size calls.
        let now = std::time::Instant::now();
        if let Some(prev) = self.restart_cooldown_at.get(&id) {
            if now.duration_since(*prev) < std::time::Duration::from_millis(1500) {
                return Ok(());
            }
        }
        let body = crate::daemon::RestartSessionBody {
            size: crate::terminal::get_size().and_then(|(cols, rows)| {
                Some(crate::daemon::TerminalSize {
                    cols: std::num::NonZeroU16::new(cols)?,
                    rows: std::num::NonZeroU16::new(rows)?,
                })
            }),
            profile: new_profile.map(str::to_owned),
            tool: new_tool.map(str::to_owned),
            extra_args: new_extra_args.map(str::to_owned),
            command_override: new_command_override.map(str::to_owned),
            unsnooze: wake_snooze,
            ..Default::default()
        };
        if let Err(error) = self
            .session_feed
            .submit(id.clone(), crate::daemon::SessionMutation::Restart(body))
        {
            self.info_dialog = Some(InfoDialog::new(
                "Restart failed",
                &format!("{error}\nReconnect the runtime and try again."),
            ));
            return Ok(());
        }
        self.restart_cooldown_at.insert(id.clone(), now);
        self.restart_in_flight.insert(id);
        Ok(())
    }

    pub(super) fn delete_selected(&mut self, options: &DeleteOptions) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            // Refuse to delete a row whose daemon start is still in flight:
            // deletion would race the start the daemon is mid-running,
            // orphaning resources non-deterministically.
            if self.restart_in_flight.contains(&id) {
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
                self.deletion_poller.request_deletion(request);
            }
        }
        Ok(())
    }

    pub(super) fn delete_selected_group(&mut self) -> anyhow::Result<()> {
        if let Some(group_path) = self.selected_group.take() {
            let owning_profile = self.selected_group_profile.take();
            let prefix = format!("{}/", group_path);
            let is_member = group_membership(&group_path, &prefix, owning_profile.as_deref());
            let ids_to_clear: Vec<String> = self
                .instances
                .values()
                .filter(|i| is_member(i))
                .map(|i| i.id.clone())
                .collect();
            self.bulk_apply_user_action(&ids_to_clear, |inst| {
                inst.group_path = String::new();
            })?;

            self.rebuild_group_trees();
            if let Some(profile) = &owning_profile {
                self.delete_group_in_profile(profile, &group_path);
            } else {
                let profiles: Vec<String> = self.group_trees.keys().cloned().collect();
                for profile in profiles {
                    self.delete_group_in_profile(&profile, &group_path);
                }
            }
            self.save()?;

            self.reload()?;
        }
        Ok(())
    }

    /// Commit one profile's group deletion before any purge is queued, so a
    /// watcher reload or restart cannot rebuild the group from an unchanged
    /// `groups.json`. Blockers are re-checked against the durable rows because
    /// a Creating member may exist only on disk. `Status::Deleting` is
    /// deliberately not persisted: it stays an in-memory overlay owned by the
    /// `PurgeTransaction` lifecycle.
    fn persist_group_delete_with_sessions(
        &mut self,
        profile: &str,
        group_path: &str,
    ) -> anyhow::Result<PersistGroupDelete> {
        let prefix = format!("{group_path}/");
        let storage = self
            .storages
            .get(profile)
            .ok_or_else(|| anyhow::anyhow!("No storage registered for profile '{profile}'"))?;
        let restart_in_flight = &self.restart_in_flight;
        let mut outcome = storage.update(|instances, groups| {
            let mut has_creating = false;
            let mut has_restarting = false;
            for instance in instances.iter().filter(|instance| {
                instance.group_path == group_path || instance.group_path.starts_with(&prefix)
            }) {
                has_creating |= instance.status == Status::Creating;
                has_restarting |= restart_in_flight.contains(&instance.id);
            }
            if has_creating {
                return Ok(PersistGroupDelete::Creating);
            }
            if has_restarting {
                return Ok(PersistGroupDelete::Restarting);
            }

            let mut members = Vec::new();
            for instance in instances.iter_mut() {
                if instance.group_path == group_path || instance.group_path.starts_with(&prefix) {
                    members.push(instance.clone());
                    instance.group_path.clear();
                }
            }
            groups.retain(|group| group.path != group_path && !group.path.starts_with(&prefix));
            Ok(PersistGroupDelete::Ready(members))
        })?;
        if let PersistGroupDelete::Ready(members) = &mut outcome {
            for instance in members {
                instance.source_profile.clear();
                instance.source_profile.push_str(profile);
            }
            if let Some(tree) = self.group_trees.get_mut(profile) {
                tree.delete_group(group_path);
            }
        }
        Ok(outcome)
    }

    pub(super) fn delete_group_with_sessions(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<()> {
        if let Some(group_path) = self.selected_group.take() {
            let owning_profile = self.selected_group_profile.take();
            let (member_ids, has_creating) = {
                let prefix = format!("{group_path}/");
                let is_member = group_membership(&group_path, &prefix, owning_profile.as_deref());
                let mut member_ids = Vec::new();
                let mut has_creating = false;
                for instance in self.instances().filter(|instance| is_member(instance)) {
                    member_ids.push(instance.id.clone());
                    has_creating |= instance.status == Status::Creating;
                }
                (member_ids, has_creating)
            };
            if has_creating {
                self.selected_group = Some(group_path);
                self.selected_group_profile = owning_profile;
                self.info_dialog = Some(InfoDialog::new(
                    "Creation in progress",
                    "A session in this group is still being created. Wait for it to finish before deleting the group.",
                ));
                return Ok(());
            }

            if member_ids
                .iter()
                .any(|session_id| self.restart_in_flight.contains(session_id))
            {
                self.selected_group = Some(group_path);
                self.selected_group_profile = owning_profile;
                self.info_dialog = Some(InfoDialog::new(
                    "Restart in progress",
                    "A session in this group is still restarting. Wait for it to finish before deleting the group.",
                ));
                return Ok(());
            }

            let profiles: Vec<String> = owning_profile
                .as_ref()
                .map(|profile| vec![profile.clone()])
                .unwrap_or_else(|| self.group_trees.keys().cloned().collect());
            let mut sessions_to_delete = Vec::new();
            for profile in profiles {
                match self.persist_group_delete_with_sessions(&profile, &group_path) {
                    Ok(PersistGroupDelete::Ready(mut members)) => {
                        sessions_to_delete.append(&mut members);
                    }
                    Ok(PersistGroupDelete::Creating) => {
                        self.selected_group = Some(group_path);
                        self.selected_group_profile = owning_profile;
                        self.info_dialog = Some(InfoDialog::new(
                            "Creation in progress",
                            "A session in this group is still being created. Wait for it to finish before deleting the group.",
                        ));
                        return Ok(());
                    }
                    Ok(PersistGroupDelete::Restarting) => {
                        self.selected_group = Some(group_path);
                        self.selected_group_profile = owning_profile;
                        self.info_dialog = Some(InfoDialog::new(
                            "Restart in progress",
                            "A session in this group is still restarting. Wait for it to finish before deleting the group.",
                        ));
                        return Ok(());
                    }
                    Err(error) => {
                        self.selected_group = Some(group_path);
                        self.selected_group_profile = owning_profile;
                        return Err(error);
                    }
                }
            }
            sessions_to_delete.sort_by(|left, right| left.id.cmp(&right.id));

            for instance in sessions_to_delete {
                let session_id = instance.id.clone();
                if let Some(current) = self.instances.get_mut(&session_id) {
                    current.status = Status::Deleting;
                    current.group_path.clear();
                }
                let delete_worktree =
                    options.delete_worktrees && instance.has_managed_worktree_or_workspace();
                let delete_branch =
                    options.delete_branches && instance.has_managed_worktree_or_workspace();
                let delete_sandbox = options.delete_containers
                    && instance
                        .sandbox_info
                        .as_ref()
                        .is_some_and(|sandbox| sandbox.enabled);
                self.deletion_poller.request_deletion(DeletionRequest {
                    session_id,
                    instance,
                    delete_worktree,
                    delete_branch,
                    delete_sandbox,
                    force_delete: options.force_delete_worktrees,
                    detach_hooks: true,
                    // No per-session keep-scratch toggle in the group-delete
                    // UX, so scratch dirs always go.
                    keep_scratch: false,
                });
            }

            self.rebuild_flat_items();
        }
        Ok(())
    }

    /// Forget a stuck deletion record; finish runtime cleanup off the input thread.
    pub(super) fn force_remove_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        let instance = self.instances.get(session_id).cloned();
        self.remove_instance(session_id);
        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;

        if let Some(instance) = instance {
            std::thread::spawn(move || {
                crate::session::deletion::cleanup_abandoned_session(instance);
            });
        }
        Ok(())
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
    ) -> anyhow::Result<()> {
        let ctx = match self.group_rename_context.take() {
            Some(ctx) => ctx,
            None => return Ok(()),
        };

        let new_path = match new_group {
            Some(g) if !g.is_empty() && g != ctx.old_path => g,
            _ if new_profile.is_none() => return Ok(()), // nothing changed
            _ => &ctx.old_path,                          // profile-only change
        };

        // Defense-in-depth: reject duplicate names (dialog validates inline, but guard here too)
        let target_profile = new_profile.unwrap_or(&ctx.old_profile);
        let profile_changed = target_profile != ctx.old_profile;
        if new_path != ctx.old_path {
            if let Some(tree) = self.group_trees.get(target_profile) {
                if tree.group_exists(new_path) {
                    anyhow::bail!(
                        "A group named '{}' already exists in profile '{}'",
                        new_path,
                        target_profile
                    );
                }
            }
        }

        if profile_changed {
            let profiles = list_profiles()?;
            if !profiles.contains(&target_profile.to_string()) {
                anyhow::bail!("Profile '{}' does not exist", target_profile);
            }
        }

        let old_prefix = format!("{}/", ctx.old_path);

        let is_member = group_membership(&ctx.old_path, &old_prefix, Some(&ctx.old_profile));
        let mut affected_ids: Vec<String> = self
            .instances
            .values()
            .filter(|instance| is_member(instance))
            .map(|instance| instance.id.clone())
            .collect();
        if profile_changed {
            affected_ids.sort();
        }

        if profile_changed {
            // Refuse every transient member before taking any guard or
            // publishing any part of the batch.
            for id in &affected_ids {
                let instance = self
                    .get_instance(id)
                    .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
                anyhow::ensure!(
                    instance.status != Status::Creating,
                    "Cannot move group while session {id} is being created"
                );
                anyhow::ensure!(
                    instance.status != Status::Deleting,
                    "Cannot move group while session {id} is being deleted"
                );
                anyhow::ensure!(
                    !instance.has_fresh_lifecycle_reservation(chrono::Utc::now()),
                    "Cannot move group while session {id} has a lifecycle operation in progress"
                );
            }

            let identity_guard = acquire_session_identity_lock()?;
            affected_ids.sort();
            let mut mutation_guards = Vec::with_capacity(affected_ids.len());
            for id in &affected_ids {
                mutation_guards.push(self.lock_session_mutation_and_reload(id)?);
                let authoritative = self
                    .get_instance(id)
                    .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
                anyhow::ensure!(
                    authoritative.status != Status::Creating,
                    "Cannot move group while session {id} is being created"
                );
                anyhow::ensure!(
                    authoritative.status != Status::Deleting,
                    "Cannot move group while session {id} is being deleted"
                );
                anyhow::ensure!(
                    !authoritative.has_fresh_lifecycle_reservation(chrono::Utc::now()),
                    "Cannot move group while session {id} has a lifecycle operation in progress"
                );
                anyhow::ensure!(
                    authoritative.source_profile == ctx.old_profile && is_member(authoritative),
                    "Group membership changed while the cross-profile move was pending"
                );
            }

            // Build the requested diffs only from rows reloaded under their
            // retained source lifecycle guards.
            let mut changes = Vec::with_capacity(affected_ids.len());
            for id in &affected_ids {
                let before = self
                    .get_instance(id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
                let new_group_path = if new_path != ctx.old_path {
                    if before.group_path == ctx.old_path {
                        new_path.to_string()
                    } else {
                        let rest = before
                            .group_path
                            .strip_prefix(&old_prefix)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Group membership changed while the cross-profile move was pending"
                                )
                            })?;
                        format!("{new_path}/{rest}")
                    }
                } else {
                    before.group_path.clone()
                };
                let mut after = before.clone();
                after.group_path = new_group_path;
                changes.push((before, after));
            }

            let target_profile = new_profile.expect("profile_changed requires a target profile");
            if !self.storages.contains_key(target_profile) {
                self.storages.insert(
                    target_profile.to_string(),
                    Storage::open(target_profile, self.file_watch.clone())?,
                );
            }
            // Run the batch transaction for its durable effect only. The moved
            // rows are republished from disk by the reload below, which also
            // re-merges runtime-only state onto them, so inserting the returned
            // rows in memory here would just be overwritten by that reload.
            {
                let source = self
                    .storages
                    .get(&ctx.old_profile)
                    .ok_or_else(|| anyhow::anyhow!("Source profile storage is not loaded"))?;
                let target = self
                    .storages
                    .get(target_profile)
                    .ok_or_else(|| anyhow::anyhow!("Target profile storage is not loaded"))?;
                let group_move = GroupMovePlan::subtree(&ctx.old_path, new_path);
                source.move_instances_to(
                    target,
                    &changes,
                    &group_move,
                    |instances, candidates| {
                        for (index, candidate) in candidates.iter().enumerate() {
                            let duplicate_in_target = is_duplicate_session(
                                instances.iter(),
                                &candidate.title,
                                &candidate.project_path,
                                None,
                            );
                            let duplicate_in_batch = is_duplicate_session(
                                candidates[..index].iter(),
                                &candidate.title,
                                &candidate.project_path,
                                None,
                            );
                            if duplicate_in_target || duplicate_in_batch {
                                return Err(duplicate_session_error(&candidate.title));
                            }
                        }
                        Ok(())
                    },
                )?;
            }
            self.reload_preserving_profile_move_runtime(&affected_ids)?;
            drop(identity_guard);
            drop(mutation_guards);
            return Ok(());
        }

        // Same-profile group edits keep their existing per-row persistence
        // behavior.
        for id in &affected_ids {
            let Some(before) = self.get_instance(id).cloned() else {
                continue;
            };
            let new_group_path = if new_path != ctx.old_path {
                if before.group_path == ctx.old_path {
                    new_path.to_string()
                } else {
                    match before.group_path.strip_prefix(&old_prefix) {
                        Some(rest) => format!("{new_path}/{rest}"),
                        None => continue,
                    }
                }
            } else {
                before.group_path
            };
            self.apply_user_action(id, |instance| {
                instance.group_path = new_group_path;
            })?;
        }

        let path_changed = new_path != ctx.old_path;

        // Capture old_path and its descendants from the pre-rebuild tree:
        // rebuild_group_trees below derives groups from instance.group_path,
        // which the loop above already migrated, so the old paths are about
        // to disappear from the in-memory tree.
        let stale_paths: Vec<String> = if path_changed || profile_changed {
            let prefix = format!("{}/", ctx.old_path);
            self.group_trees
                .get(&ctx.old_profile)
                .map(|tree| {
                    tree.get_all_groups()
                        .into_iter()
                        .map(|g| g.path)
                        .filter(|p| p == &ctx.old_path || p.starts_with(&prefix))
                        .collect()
                })
                .unwrap_or_else(|| vec![ctx.old_path.clone()])
        } else {
            Vec::new()
        };

        // Rebuild trees from the updated instance list
        self.rebuild_group_trees();

        if path_changed {
            if let Some(tree) = self.group_trees.get_mut(&ctx.old_profile) {
                tree.rename_group(&ctx.old_path, new_path);
            }
        }
        if path_changed || profile_changed {
            self.pending_group_deletions
                .entry(ctx.old_profile.clone())
                .or_default()
                .extend(stale_paths);
        }

        // When moving to a different profile, ensure the new path exists in the target tree
        if let Some(tp) = new_profile {
            if let Some(tree) = self.group_trees.get_mut(tp) {
                tree.create_group(new_path);
            }
        }

        self.save()?;
        self.reload()?;
        Ok(())
    }

    /// Edit the selected session's worktree workdir name: move the worktree
    /// directory and, optionally, rename its git branch. Persists the new
    /// `project_path` (and branch) through `apply_user_action`. See #1723.
    pub(super) fn set_worktree_name_for_selected(
        &mut self,
        new_name: &str,
        rename_branch: bool,
    ) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let live = self
            .get_instance(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let source_profile = live.source_profile.clone();
        let _identity_lock = acquire_session_identity_lock()?;
        let storage = Storage::new(&source_profile, self.file_watch.clone())?;
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&id)?;
        let authoritative_instances = storage.load()?;
        let mut authoritative = authoritative_instances
            .iter()
            .find(|instance| instance.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        authoritative.source_profile.clone_from(&source_profile);
        authoritative.merge_runtime_from_reload(&live);
        self.instances.insert(id.clone(), authoritative.clone());
        let worktree_info = authoritative.worktree_info.clone();
        let status = authoritative.status;
        let project_path = authoritative.project_path.clone();
        let is_sandboxed = authoritative.is_sandboxed();
        let Some(worktree_info) = worktree_info else {
            anyhow::bail!("Session does not use a worktree");
        };
        let duplicate_path = crate::session::worktree_edit::target_worktree_path(
            std::path::Path::new(&project_path),
            new_name,
        )
        .unwrap_or_else(|| std::path::PathBuf::from(&project_path))
        .to_string_lossy()
        .into_owned();
        if duplicate_path.trim_end_matches('/') != project_path.trim_end_matches('/')
            && is_duplicate_session(
                authoritative_instances.iter(),
                &authoritative.title,
                &duplicate_path,
                Some(&id),
            )
        {
            self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                "Rename Failed",
                &duplicate_session_error(&authoritative.title).to_string(),
            ));
            return Ok(());
        }
        if status.blocks_worktree_edit() {
            anyhow::bail!("Stop the session before editing its workdir name");
        }
        // A sandbox session keeps its container alive (running `sleep infinity`)
        // even while Idle, and that container bind-mounts the worktree dir, so
        // the `git worktree move` below would hit EBUSY, and a reused container
        // would keep mounting (and `cd`-ing into) the old path. Refuse until the
        // session is stopped, mirroring the tied-rename path. `status` alone is
        // insufficient: `blocks_worktree_edit` is false for an Idle session
        // whose container is still up. See #2117, #2414.
        // Gated on the directory actually moving: the helper discards a
        // stopped container, which is only worth doing for a real relocation.
        // A no-op or branch-only edit leaves the mount valid.
        if crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&project_path),
            new_name,
        ) && crate::session::worktree_edit::ensure_sandbox_container_released(&id, is_sandboxed)
        {
            anyhow::bail!(
                "Stop the session before editing its workdir name: its sandbox container is \
                 mounting the worktree directory"
            );
        }

        let outcome = crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info: &worktree_info,
                current_path: std::path::Path::new(&project_path),
                new_name,
                rename_branch,
            },
        )?;
        let new_path = outcome.new_path.to_string_lossy().to_string();
        let new_branch = outcome.new_branch.clone();

        // A container created against the old path is now stale: its mounts and
        // working dir are baked in at create time and do NOT follow a host-side
        // `git worktree move`, so a reused container would `docker exec -w` into
        // a path that no longer exists. Drop it to force a fresh create on next
        // start. Only when the dir actually moved; a branch-only rename leaves
        // the path valid. Mirrors `rename_selected` (#2117).
        let dir_moved = outcome.new_path != std::path::Path::new(&project_path);
        if dir_moved {
            crate::session::worktree_edit::discard_sandbox_container_after_move(&id, is_sandboxed);
        }

        self.apply_user_action(&id, |inst| {
            inst.project_path = new_path.clone();
            if let Some(branch) = &new_branch {
                if let Some(wt) = inst.worktree_info.as_mut() {
                    wt.branch = branch.clone();
                }
            }
        })?;
        drop(_identity_lock);

        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;
        Ok(())
    }

    /// Attach a repo to `id` and, when a worker is live, restart it so the agent
    /// can see the new root (#3103).
    ///
    /// The worktree is created before anything is persisted, so a save failure
    /// rolls it back rather than leaving an orphan on disk. The restart goes
    /// through the same restart marker `aoe acp restart` writes, so the daemon
    /// respawns with the stored ACP session id and the transcript survives.
    /// Dispatch an attach onto the background poller.
    ///
    /// Returns as soon as the request is queued; the outcome arrives through
    /// [`super::HomeView::apply_attach_project_results`]. An `Err` here is a
    /// refusal the caller can show immediately, so every check that can be made
    /// from the in-memory instance is made here rather than on the worker.
    pub(super) fn add_project_to_session(
        &mut self,
        id: &str,
        repo_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let Some(instance) = self.get_instance(id).cloned() else {
            anyhow::bail!("Session no longer exists");
        };
        // Defence in depth behind the picker's own gate: this is the choke point
        // both TUI entry points share, and it is what SIGTERMs the worker below.
        // See `open_add_project_for_selected` for why the check is the observed
        // status rather than the daemon's event-log probe.
        if matches!(
            instance.status,
            crate::session::Status::Creating | crate::session::Status::Deleting
        ) {
            anyhow::bail!(
                "Wait for the session to finish starting or deleting before attaching a project"
            );
        }
        // The same set the picker refuses, via the shared helper: `Waiting` and
        // `Starting` are turns in flight just as much as `Running`, and killing
        // the worker in `Waiting` discards a pending approval.
        if instance.status.blocks_worktree_edit() {
            anyhow::bail!(
                "The agent is mid-turn and attaching restarts it; wait for the turn to finish or stop the session first"
            );
        }
        // Trashed and archived too, so the set matches the picker's gate: a
        // status flip while the picker is open must not slip an attach onto a
        // session whose agent is deliberately stopped.
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
        if self.attach_project_in_flight.contains(id) {
            anyhow::bail!("An attach is already running for this session; wait for it to finish");
        }

        // Everything blocking runs on the poller thread. `git worktree add` alone
        // takes seconds, and the fetch, submodule init, worker bounce and
        // container removal behind it take longer; inline, that froze the UI for
        // the whole attach. `apply_attach_project_results` reloads and reports.
        self.attach_project_in_flight.insert(id.to_string());
        self.attach_project_poller.request_attach(
            crate::session::attach_project::AttachProjectRequest {
                session_id: id.to_string(),
                profile: instance.source_profile.clone(),
                repo_path: repo_path.to_path_buf(),
                is_sandboxed: instance.is_sandboxed(),
            },
        );
        Ok(())
    }

    pub(super) fn rename_selected(
        &mut self,
        new_title: &str,
        new_group: Option<&str>,
        new_profile: Option<&str>,
        rename_branch: bool,
    ) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            let live = self
                .get_instance(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
            let title_changed_by_user = !new_title.is_empty() && new_title != live.title;
            // The app-wide identity guard covers profile-changing renames too.
            // Existing-session guards nest beneath it in the order session
            // title -> source lifecycle -> profile Storage.
            let _identity_lock = acquire_session_identity_lock()?;
            let _mutation_guards = self.lock_session_mutation_and_reload(&id)?;
            let previous = self
                .get_instance(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
            let current_profile = previous.source_profile.clone();
            let current_title = previous.title.clone();
            let current_group = previous.group_path.clone();

            // Empty or dialog-unchanged text means preserve the authoritative
            // source title, never the snapshot captured before the locks.
            let effective_title = if !title_changed_by_user {
                current_title.clone()
            } else {
                new_title.to_string()
            };
            let effective_group = match new_group {
                None => current_group.clone(),
                Some(group) => group.to_string(),
            };

            let target_profile = new_profile.unwrap_or(&current_profile);
            if target_profile != current_profile {
                let profiles = list_profiles()?;
                if !profiles.contains(&target_profile.to_string()) {
                    anyhow::bail!("Profile '{}' does not exist", target_profile);
                }
            }

            let tied = self.tie_workdir_applies_for(&id);
            let tied_edit = tied && (current_title != effective_title || rename_branch);
            let duplicate_path = if tied_edit {
                crate::session::worktree_edit::derived_worktree_path(
                    std::path::Path::new(&previous.project_path),
                    &effective_title,
                )
            } else {
                previous.project_path.clone()
            };
            let pair_changed = current_title != effective_title
                || target_profile != current_profile
                || duplicate_path.trim_end_matches('/')
                    != previous.project_path.trim_end_matches('/');
            if pair_changed {
                let candidates = if let Some(storage) = self.storages.get(target_profile) {
                    storage.load()?
                } else {
                    Storage::open(target_profile, self.file_watch.clone())?.load()?
                };
                if is_duplicate_session(
                    candidates.iter(),
                    &effective_title,
                    &duplicate_path,
                    Some(&id),
                ) {
                    let error = duplicate_session_error(&effective_title);
                    if target_profile != current_profile {
                        return Err(error);
                    }
                    self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                        "Rename Failed",
                        &error.to_string(),
                    ));
                    return Ok(());
                }
            }

            // Tied mode (#1927): a worktree session's directory leaf follows
            // its title, so move the directory in lockstep before persisting
            // the new title. The move is gated on a stopped session; a running
            // session surfaces a warning and nothing is renamed. Applied below
            // in both the profile-move and the standard persist paths.
            let mut new_path: Option<String> = None;
            let mut new_branch: Option<String> = None;
            // Fire when the title changed (dir follows it) OR the user opted to
            let current_instance = self
                .get_instance(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
            let cross_profile_target = new_profile
                .filter(|target| *target != current_instance.source_profile.as_str())
                .map(str::to_string);
            let mut projected_move = current_instance.clone();
            projected_move.title = effective_title.clone();
            projected_move.group_path = effective_group.clone();

            if let Some(target_profile) = cross_profile_target.as_deref() {
                let profiles = list_profiles()?;
                if !profiles.contains(&target_profile.to_string()) {
                    anyhow::bail!("Profile '{}' does not exist", target_profile);
                }
                if (current_title != effective_title || rename_branch)
                    && self.tie_workdir_applies_for(&id)
                {
                    let leaf =
                        crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
                    if let Some(path) = crate::session::worktree_edit::target_worktree_path(
                        std::path::Path::new(&projected_move.project_path),
                        &leaf,
                    ) {
                        projected_move.project_path = path.to_string_lossy().to_string();
                    }
                    if rename_branch {
                        if let Some(worktree) = projected_move.worktree_info.as_mut() {
                            worktree.branch =
                                crate::session::builder::git_sanitize_branch_name(&leaf);
                        }
                    }
                }

                // Advisory preflight before any worktree, container, or branch
                // effect. The dual-locked transaction repeats this check.
                let target_storage = Storage::open(target_profile, self.file_watch.clone())?;
                let target_rows = target_storage.load()?;
                if is_duplicate_session(
                    target_rows.iter(),
                    &projected_move.title,
                    &projected_move.project_path,
                    None,
                ) {
                    return Err(duplicate_session_error(&projected_move.title));
                }
            }
            // Fire when the title changed (dir follows it) OR the user opted to
            // rename the branch (which may be requested even with the title
            // unchanged, to bring a drifted branch back in line with the dir).
            if tied_edit && cross_profile_target.is_none() {
                let snapshot = self.get_instance(&id).map(|i| {
                    (
                        i.worktree_info.clone(),
                        i.status,
                        i.project_path.clone(),
                        i.is_sandboxed(),
                    )
                });
                if let Some((Some(worktree_info), status, project_path, is_sandboxed)) = snapshot {
                    let leaf =
                        crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
                    let container_holds_worktree = !status.blocks_worktree_edit()
                        && crate::session::worktree_edit::worktree_move_required(
                            std::path::Path::new(&project_path),
                            &leaf,
                        )
                        && crate::session::worktree_edit::ensure_sandbox_container_released(
                            &id,
                            is_sandboxed,
                        );
                    if let Some(reason) =
                        worktree_rename_block(status, is_sandboxed, container_holds_worktree)
                    {
                        let body = worktree_rename_block_message(&reason);
                        self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                            "Stop the Session to Rename",
                            body,
                        ));
                        return Ok(());
                    }
                    match crate::session::worktree_edit::edit_worktree_workdir(
                        crate::session::worktree_edit::WorktreeEditRequest {
                            worktree_info: &worktree_info,
                            current_path: std::path::Path::new(&project_path),
                            new_name: &leaf,
                            rename_branch,
                        },
                    ) {
                        Ok(outcome) => {
                            let dir_moved = outcome.new_path != std::path::Path::new(&project_path);
                            new_path = Some(outcome.new_path.to_string_lossy().to_string());
                            new_branch = outcome.new_branch;
                            if dir_moved {
                                crate::session::worktree_edit::discard_sandbox_container_after_move(
                                    &id,
                                    is_sandboxed,
                                );
                            }
                        }
                        Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
                        Err(e) => {
                            self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                                "Rename Failed",
                                &format!("Could not move the worktree directory: {e}"),
                            ));
                            return Ok(());
                        }
                    }
                }
            }

            // Cross-profile worktree/container effects run inside the
            // dual-profile transaction after authoritative target validation.
            // Tmux rekeying is deliberately deferred until persistence and
            // in-memory publication have both succeeded.
            if let Some(target_profile) = cross_profile_target.as_deref() {
                if !self.storages.contains_key(target_profile) {
                    self.storages.insert(
                        target_profile.to_string(),
                        Storage::open(target_profile, self.file_watch.clone())?,
                    );
                }
                let tied_edit = (current_title != effective_title || rename_branch)
                    && self.tie_workdir_applies_for(&id);
                let effect_instance = current_instance.clone();
                let effect_id = id.clone();
                let effect_title = effective_title.clone();
                self.move_to_profile_with_effect(
                    &id,
                    target_profile,
                    projected_move,
                    Some(&current_instance),
                    false,
                    move |candidate| {
                        if tied_edit {
                            if let Some(worktree_info) = effect_instance.worktree_info.as_ref() {
                                let leaf = crate::session::worktree_edit::worktree_leaf_from_title(
                                    &effect_title,
                                );
                                let container_holds_worktree =
                                    !candidate.status.blocks_worktree_edit()
                                        && crate::session::worktree_edit::worktree_move_required(
                                            std::path::Path::new(
                                                &effect_instance.project_path,
                                            ),
                                            &leaf,
                                        )
                                        && crate::session::worktree_edit::ensure_sandbox_container_released(
                                            &effect_id,
                                            candidate.is_sandboxed(),
                                        );
                                if let Some(reason) = worktree_rename_block(
                                    candidate.status,
                                    candidate.is_sandboxed(),
                                    container_holds_worktree,
                                ) {
                                    anyhow::bail!("{}", worktree_rename_block_message(&reason));
                                }
                                match crate::session::worktree_edit::edit_worktree_workdir(
                                    crate::session::worktree_edit::WorktreeEditRequest {
                                        worktree_info,
                                        current_path: std::path::Path::new(
                                            &effect_instance.project_path,
                                        ),
                                        new_name: &leaf,
                                        rename_branch,
                                    },
                                ) {
                                    Ok(outcome) => {
                                        // The row published to the target profile
                                        // carries `candidate.project_path`, which
                                        // was precomputed by `target_worktree_path`
                                        // in `projected_move` above. `edit_worktree_workdir`
                                        // relocates the directory to its own
                                        // `outcome.new_path`, also derived by
                                        // `target_worktree_path` from the same
                                        // current path and the same title-derived
                                        // leaf, so the two are guaranteed identical
                                        // and the published row always points at the
                                        // directory that actually moved. Assert it so
                                        // any future drift in either sanitizer chain
                                        // fails loudly in debug rather than silently
                                        // stranding the row on a wrong path.
                                        debug_assert_eq!(
                                            outcome.new_path,
                                            std::path::Path::new(&candidate.project_path),
                                            "published project_path must match the moved worktree directory"
                                        );
                                        if outcome.new_path
                                            != std::path::Path::new(&effect_instance.project_path)
                                        {
                                            crate::session::worktree_edit::discard_sandbox_container_after_move(
                                                &effect_id,
                                                candidate.is_sandboxed(),
                                            );
                                        }
                                    }
                                    Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
                                    Err(error) => return Err(error.into()),
                                }
                            }
                        }
                        Ok(())
                    },
                )?;
                self.reload_preserving_profile_move_runtime(std::slice::from_ref(&id))?;
                drop(_identity_lock);
                let tmux_warning = rekey_tmux_after_persist(&id, &current_title, &effective_title);
                drop(_mutation_guards);
                if let Some(warning) = tmux_warning {
                    self.info_dialog = Some(InfoDialog::new("Rename Saved with Warning", &warning));
                }
                return Ok(());
            }

            self.apply_user_action(&id, |inst| {
                inst.title = effective_title.clone();
                inst.group_path = effective_group.clone();
                if let Some(path) = &new_path {
                    inst.project_path = path.clone();
                }
                if let Some(branch) = &new_branch {
                    if let Some(wt) = inst.worktree_info.as_mut() {
                        wt.branch = branch.clone();
                    }
                }
            })?;
            drop(_identity_lock);
            let tmux_warning = rekey_tmux_after_persist(&id, &current_title, &effective_title);
            drop(_mutation_guards);

            // Rebuild group trees and create group if needed
            self.rebuild_group_trees();
            if !effective_group.is_empty() {
                let profile = self
                    .get_instance(&id)
                    .map(|i| i.source_profile.clone())
                    .unwrap_or_else(|| self.config_profile());
                if let Some(tree) = self.group_trees.get_mut(&profile) {
                    tree.create_group(&effective_group);
                }
            }
            self.save()?;
            self.reload()?;
            if let Some(warning) = tmux_warning {
                self.info_dialog = Some(InfoDialog::new("Rename Saved with Warning", &warning));
            }
        }
        Ok(())
    }

    /// Open the duration picker, or queue an unsnooze for an already snoozed row.
    pub(super) fn toggle_snooze_at_cursor(&mut self) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let Some(instance) = self.instances.get(&id) else {
            return Ok(());
        };
        if instance.is_snoozed() {
            return self.session_feed.submit(
                id,
                crate::daemon::SessionMutation::Snooze(crate::daemon::UpdateSnoozeBody {
                    minutes: None,
                }),
            );
        }
        self.snooze_duration_dialog = Some(crate::tui::dialogs::SnoozeDurationDialog::new(
            &instance.title,
        ));
        self.pending_snooze_session = Some(id);
        Ok(())
    }

    pub(super) fn snooze_session_for(&mut self, id: &str, minutes: u32) -> anyhow::Result<()> {
        self.session_feed.submit(
            id.to_owned(),
            crate::daemon::SessionMutation::Snooze(crate::daemon::UpdateSnoozeBody {
                minutes: Some(minutes),
            }),
        )?;
        if self.sort_order == crate::session::config::SortOrder::Attention {
            self.select_top_attention(Some(id));
        }
        Ok(())
    }

    /// Queue a favorite change; only canonical snapshots update the row.
    pub(super) fn toggle_favorite_at_cursor(&mut self) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let is_fav = match self.instances.get(&id) {
            Some(i) => i.is_favorited(),
            None => return Ok(()),
        };
        self.session_feed.submit(
            id,
            crate::daemon::SessionMutation::Favorite(crate::daemon::UpdateFavoriteBody {
                favorited: !is_fav,
            }),
        )?;
        Ok(())
    }

    /// The session the cursor should land on after the cursor's row is
    /// archived away: the nearest non-archived session below the cursor,
    /// else the nearest one above. `None` when no other active session is
    /// VISIBLE (the caller falls back to an index clamp); active sessions
    /// hidden inside collapsed groups are deliberately not candidates, so
    /// archiving never yanks the cursor into a group the user folded away.
    /// Scans the pre-archive flat list, so it walks the rows the
    /// user sees; archived rows already parked under the Archived section
    /// are skipped so the cursor never advances into it.
    fn archive_successor_session(&self, archiving_id: &str) -> Option<String> {
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

    /// Queue a manual unread change and hold its mark for this visit.
    pub(super) fn toggle_unread_at_cursor(&mut self) -> anyhow::Result<()> {
        if !crate::session::unread_enabled() {
            return Ok(());
        }
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let Some(instance) = self.instances.get(&id) else {
            return Ok(());
        };
        let unread = !instance.is_unread();
        self.session_feed.submit(
            id.clone(),
            crate::daemon::SessionMutation::Unread(crate::daemon::UpdateUnreadBody { unread }),
        )?;
        if unread {
            self.manual_unread_hold = Some(id);
        } else if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            self.manual_unread_hold = None;
        }
        Ok(())
    }

    /// Toggle the cursor's session: archive or unarchive. Archive tears down
    /// all tmux sessions (agent + ancillary); worktree, branch, container
    /// preserved. Unarchive does NOT respawn; press `e` to restart, or send
    /// a message to auto-unarchive. See #1868.
    pub(super) fn toggle_archive_at_cursor(&mut self) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        // The shelve/unshelve key doubles as restore for the Trash section: a
        // trashed row can't be meaningfully archived, so `z` on it pulls the
        // session back out of the trash instead. See #2489.
        if matches!(self.instances.get(&id), Some(i) if i.is_trashed()) {
            self.restore_selected_from_trash();
            return Ok(());
        }
        let is_archived = match self.instances.get(&id) {
            Some(i) => i.is_archived(),
            None => return Ok(()),
        };
        if is_archived {
            self.session_feed.submit(
                id.clone(),
                crate::daemon::SessionMutation::Archive(crate::daemon::UpdateArchiveBody {
                    archived: false,
                    kill_pane: true,
                }),
            )?;
            // The row rises when the snapshot proves it; the cursor follows
            // then, because after the rebuild the row jumps from tier 99 to its
            // real tier and would otherwise strand the cursor at the old index.
            self.pending_archive_cursor = Some(PendingArchiveCursor {
                id,
                archived: false,
                successor: None,
            });
            return Ok(());
        }

        // Decide where the cursor lands BEFORE the row sinks, against the
        // pre-archive list the user is actually looking at. Only the
        // non-Attention branch consumes it; Attention re-picks from the top.
        let successor = (self.sort_order != crate::session::config::SortOrder::Attention)
            .then(|| self.archive_successor_session(&id))
            .flatten();

        // The daemon tears the panes down (#1868) and stamps `archived_at`; the
        // row, the section counts and the cursor placement follow from the
        // canonical snapshot, which is why the decision above is remembered.
        self.session_feed.submit(
            id.clone(),
            crate::daemon::SessionMutation::Archive(crate::daemon::UpdateArchiveBody {
                archived: true,
                kill_pane: true,
            }),
        )?;
        self.pending_archive_cursor = Some(PendingArchiveCursor {
            id,
            archived: true,
            successor,
        });
        Ok(())
    }
    /// Move a session to the trash and set `trashed_at`. Durable artifacts are
    /// kept so it can be restored. The Trash section's collapse state is left
    /// untouched: like single-row archive, the section header's count is the
    /// feedback, so a user who collapsed it stays collapsed (#2489).
    ///
    /// The durable trash marker is written inline (a fast local write) so the
    /// row flips to Trashed immediately. Everything that can block, tmux
    /// teardown, the sandbox container stop, and the worktree relocation out of
    /// the active dir, runs off-thread on the `TrashPoller` and is reconciled
    /// by [`apply_trash_results`](crate::tui::home::HomeView::apply_trash_results).
    /// Stop the container before relocating its live bind-mounted worktree.
    pub(super) fn trash_session_by_id(&mut self, id: &str) {
        let Some((profile, mut request_instance)) = self
            .instances
            .get(id)
            .map(|instance| (instance.source_profile.clone(), instance.clone()))
        else {
            return;
        };
        let Some(storage) = self.storages.get(&profile) else {
            tracing::warn!(
                target: "tui.session",
                session = %id,
                "trash failed: no storage registered for profile {profile}"
            );
            return;
        };
        let acquisition = (|| -> anyhow::Result<_> {
            let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(id)?;
            storage.update(|instances, _groups| {
                let stored = instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                    .ok_or_else(|| anyhow::anyhow!("session disappeared before trash"))?;
                let generation = stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)?;
                stored.trash();
                Ok((generation, stored.lifecycle_reservation.clone()))
            })
        })();
        let (generation, reservation) = match acquisition {
            Ok(acquired) => acquired,
            Err(error) => {
                tracing::warn!(target: "tui.session", session = %id, "trash failed: {error}");
                return;
            }
        };

        request_instance.trash();
        request_instance.lifecycle_generation = generation;
        request_instance.lifecycle_reservation = reservation.clone();
        if let Some(instance) = self.instances.get_mut(id) {
            instance.trash();
            instance.lifecycle_generation = generation;
            instance.lifecycle_reservation = reservation;
        }
        self.trash_poller
            .request_trash(crate::session::trash::TrashRequest {
                session_id: id.to_string(),
                instance: request_instance,
                generation,
            });
        self.rebuild_flat_items();
        self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
        self.update_selected();
    }

    /// Restore the selected trashed session, clearing `trashed_at` so it
    /// returns to its prior bucket. No-op when the selection is not trashed.
    /// The session stays stopped (trash killed its panes); the user restarts
    /// it with `e` like any stopped session. See #2489.
    pub(super) fn restore_selected_from_trash(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some((profile, owned_trash_generation)) =
            self.instances.get(&id).filter(|i| i.is_trashed()).map(|i| {
                let generation = i
                    .lifecycle_reservation
                    .as_ref()
                    .filter(|reservation| reservation.op == LifecycleOperation::Trash)
                    .map(|reservation| reservation.generation);
                (i.source_profile.clone(), generation)
            })
        else {
            return;
        };
        // Restore bypasses the generic user-action diff because lifecycle
        // ownership, worktree movement, and durable untrash must stay under the
        // per-instance flock.
        let outcome = {
            let Some(storage) = self.storages.get(&profile) else {
                tracing::warn!(
                    target: "tui.home",
                    profile = %profile,
                    id = %id,
                    "restore: no storage registered for profile"
                );
                return;
            };
            restore_from_trash_with_storage(storage, &id, owned_trash_generation)
        };
        match outcome {
            RestoreFromTrash::Restored {
                project_path,
                pre_trash_project_path,
            } => {
                if let Some(inst) = self.instances.get_mut(&id) {
                    inst.project_path = project_path;
                    inst.pre_trash_project_path = pre_trash_project_path;
                    inst.untrash();
                    inst.status = Status::Stopped;
                    inst.lifecycle_reservation = None;
                }
                self.rebuild_flat_items();
                self.select_session_by_id(&id);
            }
            RestoreFromTrash::AlreadyGone => {
                self.drop_peer_deleted_rows(std::slice::from_ref(&id));
                self.rebuild_flat_items();
            }
            RestoreFromTrash::Busy(reason) => {
                self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                    "Restore Failed",
                    &format!("Session is {reason}, so it was not restored."),
                ));
            }
            RestoreFromTrash::WorktreeFailed { reason } => {
                self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                    "Restore Failed",
                    &format!("Could not restore the worktree: {reason}"),
                ));
            }
            RestoreFromTrash::PersistFailed => {
                self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                    "Restore Failed",
                    "Could not persist the restore. Try again.",
                ));
            }
        }
    }

    /// Restore every trashed session back into its group. The synthetic Trash
    /// section's "Restore All" bulk action: drives each row through the same
    /// per-row `restore_selected_from_trash` (claim, off-lock worktree move,
    /// untrash) so the claim/commit races (#2541) are handled identically to a
    /// single restore. Each row's failure surfaces its own info dialog; the
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
            // `restore_selected_from_trash` acts on the selection, so point it
            // at each row in turn; it re-selects the restored session on
            // success, and the next iteration overwrites that.
            self.selected_session = Some(id);
            self.restore_selected_from_trash();
        }
    }

    /// Unarchive every archived session. The synthetic Archived section's
    /// "Restore All" bulk action. Archived rows stay Stopped (archiving killed
    /// their panes); the user restarts them with `e` when wanted, same as any
    /// single unarchive. Reversible, so no confirmation upstream.
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

    /// Permanently purge every trashed session. The Trash section's "Empty
    /// Trash" bulk action, reached only after the confirm dialog. Each row runs
    /// the same off-thread deletion path as a single permanent delete:
    /// reservation, hooks, teardown, and durable completion all run inside
    /// `deletion_poller`; the event loop only queues requests.
    /// Cleanup options are resolved per row from its repo config, mirroring the
    /// CLI `empty-trash`, with force removal so a dirty worktree can't keep a
    /// row pinned.
    pub(super) fn empty_trash_all(&mut self) {
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
            // A daemon start still in flight would race the teardown against
            // the container it is mid-creating; skip that row rather than
            // orphan resources, the same guard `delete_selected` applies to
            // a single delete.
            if self.restart_in_flight.contains(&id) {
                continue;
            }

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

            self.deletion_poller.request_deletion(DeletionRequest {
                session_id: id.clone(),
                instance: inst.clone(),
                delete_worktree,
                delete_branch,
                delete_sandbox,
                force_delete: true,
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

    /// Collect the active (non-archived) session ids under the currently
    /// selected group header, honoring the active group-by mode. Archived
    /// sessions are excluded: they already live under the synthetic Archived
    /// section, and re-archiving them is a no-op. Returns empty when no group
    /// is selected.
    pub(super) fn active_sessions_in_selected_group(&self) -> Vec<String> {
        let Some(group_path) = self.selected_group.as_deref() else {
            return Vec::new();
        };
        match self.group_by {
            // Project headers are derived from each session's repo name and
            // unified across profiles, narrowed only by the active profile
            // filter, exactly as `build_flat_items_by_project` builds them.
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
            // Org headers are derived from each session's resolved remote
            // owner key (host-scoped, not just the bare owner, so same-named
            // owners on different hosts stay separate), same
            // unification-across-profiles rationale as Project.
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
            // Manual groups can nest, so a session belongs when its path
            // matches exactly or sits beneath the group. Scope to the group's
            // owning profile the same way `delete_selected_group` does.
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

    /// Archive every active session under the selected group: tmux teardown
    /// runs off-thread, persist runs inline. Confirmation upstream. See #1868.
    pub(super) fn archive_selected_group(&mut self) -> anyhow::Result<()> {
        let ids = self.active_sessions_in_selected_group();
        if ids.is_empty() {
            return Ok(());
        }
        // Off-thread tmux teardown so N x 4 shellouts don't block the input
        // thread. Mirrors `force_remove_session`.
        let kill_targets: Vec<_> = ids
            .iter()
            .filter_map(|id| self.instances.get(id).cloned())
            .collect();
        std::thread::spawn(move || {
            for inst in kill_targets {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inst.kill_all_tmux_sessions()
                })) {
                    tracing::error!(
                        target: "session.tmux_cleanup",
                        session_id = %inst.id,
                        "archive_selected_group tmux teardown panicked: {:?}",
                        panic
                    );
                }
            }
        });
        self.bulk_apply_user_action(&ids, |inst| inst.archive())?;
        self.reveal_archived_section();
        self.rebuild_flat_items();
        // The project header vanishes once its last active member is archived
        // (project headers are seeded from live sessions only), so the cursor's
        // old index may now point past the list end; clamp and re-resolve.
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
        Ok(())
    }
}

/// Outcome of a TUI restore-from-trash driven directly against storage. See #2541.
enum RestoreFromTrash {
    Restored {
        project_path: String,
        pre_trash_project_path: Option<String>,
    },
    AlreadyGone,
    Busy(String),
    WorktreeFailed {
        reason: String,
    },
    PersistFailed,
}

/// Restore under identity and lifecycle exclusion through durable commit.
fn restore_from_trash_with_storage(
    storage: &Storage,
    id: &str,
    owned_trash_generation: Option<u64>,
) -> RestoreFromTrash {
    let _identity = match crate::session::acquire_session_identity_lock() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore identity lock failed: {error}");
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
    let decision = match storage.update(|instances, _groups| {
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

    let loaded = match storage.load() {
        Ok(all) => all.into_iter().find(|instance| instance.id == id),
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore load failed: {error}");
            let _ = storage.update(|instances, _groups| {
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
    let Some(mut instance) = loaded else {
        return RestoreFromTrash::AlreadyGone;
    };

    if let crate::session::trash::RestoreOutcome::Failed { reason } =
        crate::session::trash::restore_worktree_location(&mut instance)
    {
        let _ = storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|candidate| candidate.id == id) {
                stored.release_lifecycle_reservation_if_owned(
                    LifecycleOperation::Restore,
                    generation,
                );
            }
            Ok(())
        });
        return RestoreFromTrash::WorktreeFailed { reason };
    }
    let restored_path = instance.project_path.clone();
    let restored_pre = instance.pre_trash_project_path.clone();

    match storage.update(|instances, _groups| {
        Ok(crate::session::claim::finalize_restore_commit(
            instances,
            id,
            generation,
            &restored_path,
            &restored_pre,
        ))
    }) {
        Ok(crate::session::claim::RestoreCommit::Committed) => RestoreFromTrash::Restored {
            project_path: restored_path,
            pre_trash_project_path: restored_pre,
        },
        Ok(crate::session::claim::RestoreCommit::Superseded) => {
            RestoreFromTrash::Busy(crate::session::NEWER_GENERATION_BUSY_REASON.to_string())
        }
        Ok(crate::session::claim::RestoreCommit::AlreadyGone) => RestoreFromTrash::AlreadyGone,
        Err(error) => {
            tracing::warn!(target: "tui.home", id = %id, "restore commit failed: {error}");
            RestoreFromTrash::PersistFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An Idle sandbox session whose container is still running is the #1927
    // follow-up bug: the worktree dir is an active bind-mount source, so
    // `git worktree move` fails with EBUSY. Before the fix this returned
    // `None` (the rename proceeded and the move blew up with "fatal: failed
    // to move"); it must now block with the sandbox-specific reason.
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
    fn idle_non_sandbox_is_safe() {
        // No container, nothing holds the dir; the move proceeds.
        assert_eq!(worktree_rename_block(Status::Idle, false, false), None);
    }

    #[test]
    fn active_status_blocks_as_active_agent() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
            Status::Deleting,
        ] {
            assert_eq!(
                worktree_rename_block(status, false, false),
                Some(WorktreeRenameBlock::ActiveAgent),
                "{status:?} should block as ActiveAgent"
            );
        }
    }

    #[test]
    fn active_status_takes_precedence_over_container() {
        // A busy agent reports as ActiveAgent even on a sandbox session with a
        // live container; status is checked first.
        assert_eq!(
            worktree_rename_block(Status::Running, true, true),
            Some(WorktreeRenameBlock::ActiveAgent)
        );
    }
}

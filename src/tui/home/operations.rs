//! Session operations for HomeView (create, delete, rename)

use crate::session::builder::{self, InstanceParams};
use crate::session::{list_profiles, GroupTree, Item, Status, Storage};
use crate::tui::deletion_poller::DeletionRequest;
use crate::tui::dialogs::{DeleteOptions, GroupDeleteOptions, NewSessionData};

use super::HomeView;

/// Compact human readable label for the snooze status line (`"30 min"`,
/// `"1 hr"`, `"24 hr"`, `"2 hr 30 min"`). The picker only ever submits
/// 30 / 60 / 1440, but formatting is kept general so arbitrary values
/// from other callers read cleanly too.
fn humanize_minutes(m: u32) -> String {
    let hours = m / 60;
    let mins = m % 60;
    match (hours, mins) {
        (0, _) => format!("{} min", mins),
        (_, 0) => format!("{} hr", hours),
        _ => format!("{} hr {} min", hours, mins),
    }
}

impl HomeView {
    /// Pin or unpin the project header under the cursor (project view only).
    ///
    /// Pinning registers the repo in the global project registry (the same
    /// store the WebUI writes), so the project keeps its header in project
    /// view even after its last session is deleted. Unpinning removes the
    /// registry entry; a project with no remaining sessions then disappears.
    ///
    /// The registry is the shared persistence layer: this goes through the
    /// same `projects::add` / `projects::remove` the web API and the projects
    /// dialog use, so canonicalization and conflict rules stay in one place.
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
            // back to the basename match (it exists only because a registered
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
            // Unpin means "this repo is no longer pinned anywhere", so clear
            // every registry entry for its canonical path rather than just the
            // one `load_merged` happened to surface. A path can sit in more than
            // one scope at once (`--allow-override` lets a profile entry shadow
            // a global one); removing only the visible entry would re-surface
            // the shadowed one and leave the header pinned after a "success"
            // dialog. `registered_projects` also drops which profile each entry
            // came from in all-profiles mode, and `config_profile()` is only
            // the default, so sweep the global file plus every loaded profile.
            let target = existing.path.clone();
            let mut profiles: Vec<String> = self.storages.keys().cloned().collect();
            if !profiles.contains(&profile) {
                profiles.push(profile.clone());
            }
            // Global lives in one shared file, so the profile arg is irrelevant.
            let mut removals = vec![projects::remove(&profile, ProjectScope::Global, &target)];
            for p in &profiles {
                removals.push(projects::remove(p, ProjectScope::Profile, &target));
            }
            let mut removed_any = false;
            let mut hard_err: Option<projects::RegistryError> = None;
            for res in removals {
                match res {
                    Ok(_) => removed_any = true,
                    Err(projects::RegistryError::NotFound(_)) => {}
                    Err(e) => hard_err = Some(e),
                }
            }
            // Surface a real I/O/parse failure even if some entry was removed;
            // a partial unpin the user can't see is worse than a visible error.
            let result: Result<(), projects::RegistryError> = match (hard_err, removed_any) {
                (Some(e), _) => Err(e),
                (None, true) => Ok(()),
                (None, false) => Err(projects::RegistryError::NotFound(format!(
                    "No pinned project '{}' found in any loaded scope",
                    label
                ))),
            };
            match result {
                Ok(_) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Project Unpinned",
                        &format!(
                            "'{}' is no longer pinned. It will drop from project view once it has no sessions.",
                            label
                        ),
                    ));
                }
                Err(e) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Unpin Failed",
                        &format!("Could not unpin: {}", e),
                    ));
                }
            }
        } else {
            // Pin the repo backing this header. An unpinned header always has at
            // least one live session (an empty header is pinned by
            // construction), so its repo path is known.
            let Some(repo_path) = header_path else {
                return;
            };
            let project = Project::new(label.clone(), repo_path, ProjectScope::Global);
            match projects::add(&profile, ProjectScope::Global, project, false) {
                Ok(_) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Project Pinned",
                        &format!(
                            "'{}' is pinned. It will stay in project view even with no sessions.",
                            label
                        ),
                    ));
                }
                Err(e) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Pin Failed",
                        &format!("Could not pin: {}", e),
                    ));
                }
            }
        }

        self.refresh_registered_projects();
        self.flat_items = self.build_flat_items();
        self.update_selected();
    }

    pub(super) fn create_session(&mut self, data: NewSessionData) -> anyhow::Result<String> {
        let target_profile = data.profile.clone();

        // In unified mode, all instances are loaded, so use them for title dedup.
        // For the target profile, filter to that profile's instances.
        let existing_titles: Vec<&str> = self
            .instances()
            .iter()
            .filter(|i| i.source_profile == target_profile)
            .map(|i| i.title.as_str())
            .collect();
        let existing_branches: Vec<&str> = self
            .instances()
            .iter()
            .filter(|i| i.source_profile == target_profile)
            .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
            .collect();

        let params = InstanceParams {
            title: data.title,
            path: data.path,
            group: data.group,
            tool: data.tool,
            worktree_enabled: data.worktree_enabled,
            worktree_branch: data.worktree_branch,
            create_new_branch: data.create_new_branch,
            base_branch: data.base_branch,
            sandbox: data.sandbox,
            sandbox_image: data.sandbox_image,
            yolo_mode: data.yolo_mode,
            extra_env: data.extra_env,
            extra_args: data.extra_args,
            command_override: data.command_override,
            extra_repo_paths: data.extra_repo_paths,
            scratch: data.scratch,
        };

        let build_result = builder::build_instance(
            params,
            &existing_titles,
            &existing_branches,
            &target_profile,
        )?;
        let mut instance = build_result.instance;
        instance.source_profile = target_profile.clone();
        let session_id = instance.id.clone();

        // Ensure target profile storage exists
        if !self.storages.contains_key(&target_profile) {
            self.storages.insert(
                target_profile.clone(),
                Storage::new(&target_profile, self.file_watch.clone())?,
            );
        }

        self.add_instance(instance.clone());
        self.rebuild_group_trees();
        if !instance.group_path.is_empty() {
            if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                tree.create_group(&instance.group_path);
            }
        }
        self.save()?;

        self.reload()?;
        // Same rationale as the async branch in apply_creation_results:
        // reload()'s restore-previous-selection fallback lands the cursor
        // on whichever flat_items index is closest to the previously-
        // selected row, which in project-grouped layouts is often the
        // new session's group folder. Pin selection here so the caller
        // (Action::AttachAfterCreate) sees the new session as the
        // visible row and the user's not staring at the wrong preview.
        self.select_and_reveal_session(&session_id);
        Ok(session_id)
    }

    /// Restart the cursor's session, optionally migrating to a new profile
    /// and/or swapping the AI engine first.
    ///
    /// Guards (apply to bare `e` / `E` / `F5` and dialog-submitted restarts):
    /// - No selection: no-op.
    /// - Transient lifecycle (`Creating` / `Deleting`): drop.
    /// - Sunk rows: archived and pane-dead always drop (archive's contract
    ///   is "do not auto-revive"; dead panes have a dedicated revive path).
    ///   Snoozed rows drop only when `sort_order == Attention`; in other
    ///   sort modes the snooze surface is hidden, so silently swallowing
    ///   the press would leave the user staring at a row that looks
    ///   restartable but isn't. Outside Attention we clear the snooze flag
    ///   and let the restart proceed so behavior matches what the user
    ///   sees on screen.
    /// - Spam-debounce: if the same session was restarted within the last
    ///   1.5s, the press is dropped. Without this guard rapid `e` presses
    ///   would each spawn a wake-up worker AND tear down the still-booting
    ///   tmux pane via overlapping `restart_with_size` calls.
    ///
    /// `new_profile`: when `Some(p)` and `p` differs from the current
    /// `source_profile`, the session moves between profile storages.
    /// Mirrors the profile-move path in `rename_selected` so a restart-
    /// with-different-profile behaves the same as rename + restart.
    ///
    /// `new_tool`: when `Some(t)` and `t` differs from the current `tool`,
    /// the field is updated before respawn so the new agent binary starts
    /// on the next launch.
    ///
    /// Restart goes through `try_mutate_instance_writeback_on_err` so all
    /// of `restart_with_size`'s mutations (cleared stale `agent_session_id`
    /// on Tier-2 resume fallback, `last_accessed_at` bumps, etc.) are
    /// preserved on the live instance.
    ///
    /// The wake-up message is read from the resolved config
    /// (`session.restart_wake_message`); an empty value disables the
    /// wake-up entirely while still running the restart.
    ///
    /// The readiness probe + send-keys runs on a background OS thread so
    /// the TUI event loop never blocks.
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

        // Skip transient + sunk rows. Pull the snapshot details we need on
        // the worker thread in the same borrow so we don't re-look up the
        // instance under different conditions later. Snoozed rows only
        // skip when the user is in Attention sort; see method doc.
        let in_attention = self.sort_order == crate::session::config::SortOrder::Attention;
        let (skip, wake_snooze, title, tool) = match self.get_instance(&id) {
            Some(inst) => {
                let snoozed = inst.is_snoozed();
                let skip = matches!(inst.status, Status::Creating | Status::Deleting)
                    || inst.is_archived()
                    || (snoozed && in_attention)
                    || inst.pane_dead_observed;
                let wake_snooze = snoozed && !in_attention;
                (skip, wake_snooze, inst.title.clone(), inst.tool.clone())
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
        self.restart_cooldown_at.insert(id.clone(), now);

        // Outside Attention sort, restart on a snoozed row clears the
        // snooze flag so the persisted state matches what the user sees
        // after the wake-up (a Running row, no snooze badge). Sequenced
        // after the debounce so a press dropped by the cooldown doesn't
        // clear snooze without restarting.
        if wake_snooze {
            self.mutate_instance(&id, |inst| inst.unsnooze());
        }

        // Apply tool swap before restart so the new binary starts on the
        // next launch.
        if let Some(target_tool) = new_tool {
            let current_tool = self
                .get_instance(&id)
                .map(|i| i.tool.clone())
                .unwrap_or_default();
            if target_tool != current_tool {
                self.mutate_instance(&id, |inst| {
                    inst.tool = target_tool.to_string();
                });
            }
        }

        // Apply command override + extra args swaps before restart so the
        // adjusted launch command takes effect on the next spawn. Both come
        // pre-resolved from the restart dialog (which re-seeds them from the
        // selected tool's config when the engine is swapped), so we set the
        // instance fields directly. `None` means "leave as-is".
        if let Some(command) = new_command_override {
            self.mutate_instance(&id, |inst| {
                inst.command = command.to_string();
            });
        }
        if let Some(extra) = new_extra_args {
            self.mutate_instance(&id, |inst| {
                inst.extra_args = extra.to_string();
            });
        }

        // Apply profile move. Validates the target exists, lazily creates
        // its Storage, and rebuilds group trees so the row renders under
        // the new profile immediately.
        if let Some(target_profile) = new_profile {
            let current_profile = self
                .get_instance(&id)
                .map(|i| i.source_profile.clone())
                .unwrap_or_else(|| {
                    self.active_profile
                        .clone()
                        .unwrap_or_else(|| "default".to_string())
                });
            if target_profile != current_profile {
                let profiles = list_profiles()?;
                if !profiles.contains(&target_profile.to_string()) {
                    anyhow::bail!("Profile '{}' does not exist", target_profile);
                }
                if !self.storages.contains_key(target_profile) {
                    self.storages.insert(
                        target_profile.to_string(),
                        Storage::new(target_profile, self.file_watch.clone())?,
                    );
                }
                if !self.group_trees.contains_key(target_profile) {
                    self.group_trees.insert(
                        target_profile.to_string(),
                        GroupTree::new_with_groups(&[], &[]),
                    );
                }
                // Capture the moved row's old group_path before the move so
                // we can prune the source profile's now-empty copy after.
                // Without the prune, the source profile retains an empty
                // group header with the same name as the one the row appears
                // under in the target profile, which reads as a duplicate
                // group in unified view.
                let old_group_path = self
                    .get_instance(&id)
                    .map(|i| i.group_path.clone())
                    .unwrap_or_default();
                self.move_to_profile(&id, target_profile, old_group_path.clone())?;
                self.prune_empty_group(&current_profile, &old_group_path);
                self.rebuild_group_trees();
                // Rebuild the visible row list too; otherwise the row still
                // renders under the old profile until the next reload, and
                // any follow-up keybind hits stale cursor state.
                self.flat_items = self.build_flat_items();
            }
        }

        // Restart the live instance (not a detached clone) so all
        // non-status fields restart_with_size touches are kept.
        let size = crate::terminal::get_size();
        self.try_mutate_instance_writeback_on_err(&id, |inst| {
            inst.restart_with_size(size).map(|_| ())
        })?;

        // Stamp touch_last_accessed on the user's gesture (the row should
        // visibly bump immediately). save() pushes both the restart-side
        // mutations and the touch.
        self.mutate_instance(&id, |inst| inst.touch_last_accessed());
        self.save()?;

        // Resolve the wake message via the moved session's profile config
        // (already merges global + profile overrides). Empty string is the
        // documented opt-out.
        let profile = self
            .get_instance(&id)
            .map(|i| i.source_profile.clone())
            .unwrap_or_else(|| {
                self.active_profile
                    .clone()
                    .unwrap_or_else(|| "default".to_string())
            });
        let wake_msg = crate::session::resolve_config(&profile)
            .map(|c| c.session.restart_wake_message.clone())
            .unwrap_or_else(|_| "wake up: pick up what you were doing".to_string());
        if wake_msg.is_empty() {
            return Ok(());
        }

        // Background worker: wait for the pane to be live + past the boot
        // shell, then send the wake-up keys. Failure to even spawn is
        // logged so the user can correlate a missing wake-up with a real
        // OS-level failure rather than silent loss.
        let worker_session_id = id.clone();
        let worker_title = title;
        let worker_tool = tool;
        let spawn_result = std::thread::Builder::new()
            .name(format!("aoe-restart-wake/{}", id))
            .stack_size(128 * 1024)
            .spawn(move || {
                let Ok(tmux_session) = crate::tmux::Session::new(&worker_session_id, &worker_title)
                else {
                    return;
                };
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
                loop {
                    if !tmux_session.exists() {
                        return;
                    }
                    let pane_alive = !tmux_session.is_pane_dead();
                    let hook_active = crate::hooks::read_hook_status(&worker_session_id).is_some();
                    if pane_alive && (hook_active || !tmux_session.is_pane_running_shell()) {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                if !tmux_session.exists() {
                    return;
                }
                let delay = crate::agents::send_keys_enter_delay(&worker_tool);
                if let Err(e) = tmux_session.send_keys_with_delay(&wake_msg, delay) {
                    tracing::warn!("failed to send wake-up message after restart: {}", e);
                }
            });
        if let Err(err) = spawn_result {
            tracing::warn!(?err, "failed to spawn restart wake-up worker");
        }
        Ok(())
    }

    pub(super) fn delete_selected(&mut self, options: &DeleteOptions) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

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
            let ids_to_clear: Vec<String> = self
                .instances
                .iter()
                .filter(|i| {
                    (i.group_path == group_path || i.group_path.starts_with(&prefix))
                        && owning_profile
                            .as_ref()
                            .is_none_or(|p| p == &i.source_profile)
                })
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

    pub(super) fn delete_group_with_sessions(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<()> {
        if let Some(group_path) = self.selected_group.take() {
            let owning_profile = self.selected_group_profile.take();
            let prefix = format!("{}/", group_path);

            let sessions_to_delete: Vec<String> = self
                .instances()
                .iter()
                .filter(|i| {
                    (i.group_path == group_path || i.group_path.starts_with(&prefix))
                        && owning_profile
                            .as_ref()
                            .is_none_or(|p| p == &i.source_profile)
                })
                .map(|i| i.id.clone())
                .collect();

            self.bulk_apply_user_action(&sessions_to_delete, |inst| {
                inst.status = Status::Deleting;
                inst.group_path = String::new();
            })?;

            for session_id in &sessions_to_delete {
                if let Some(inst) = self.get_instance(session_id) {
                    let delete_worktree = options.delete_worktrees
                        && (inst
                            .worktree_info
                            .as_ref()
                            .is_some_and(|wt| wt.managed_by_aoe)
                            || inst
                                .workspace_info
                                .as_ref()
                                .is_some_and(|ws| ws.cleanup_on_delete));
                    let delete_branch = options.delete_branches
                        && (inst
                            .worktree_info
                            .as_ref()
                            .is_some_and(|wt| wt.managed_by_aoe)
                            || inst
                                .workspace_info
                                .as_ref()
                                .is_some_and(|ws| ws.cleanup_on_delete));
                    let delete_sandbox = options.delete_containers
                        && inst.sandbox_info.as_ref().is_some_and(|s| s.enabled);
                    let request = DeletionRequest {
                        session_id: session_id.clone(),
                        instance: inst.clone(),
                        delete_worktree,
                        delete_branch,
                        delete_sandbox,
                        force_delete: options.force_delete_worktrees,
                        detach_hooks: true,
                        // Group-delete UX doesn't have a per-session
                        // keep-scratch toggle; scratch dirs in a group
                        // delete are removed unconditionally.
                        keep_scratch: false,
                    };
                    self.deletion_poller.request_deletion(request);
                }
            }

            if let Some(profile) = &owning_profile {
                self.delete_group_in_profile(profile, &group_path);
            } else {
                let profiles: Vec<String> = self.group_trees.keys().cloned().collect();
                for profile in profiles {
                    self.delete_group_in_profile(&profile, &group_path);
                }
            }
            self.save()?;
            self.flat_items = self.build_flat_items();
        }
        Ok(())
    }

    /// Force-remove a session from storage. Worktree, branch, and
    /// container cleanup are skipped (the original deletion already
    /// attempted them); tmux teardown is fired off-thread so a hung
    /// tmux call cannot block the storage update on the TUI input
    /// thread. Used for sessions stuck in the Deleting state where
    /// the background deletion thread never returned a result.
    pub(super) fn force_remove_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        if let Some(inst) = self.instances.iter().find(|i| i.id == session_id) {
            let inst = inst.clone();
            std::thread::spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inst.kill_all_tmux_sessions()
                })) {
                    tracing::error!(
                        target: "session.tmux_cleanup",
                        session_id = %inst.id,
                        "force_remove tmux teardown panicked: {:?}",
                        panic
                    );
                }
            });
        }
        self.remove_instance(session_id);
        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;
        Ok(())
    }

    pub(super) fn group_has_managed_worktrees(&self, group_path: &str, prefix: &str) -> bool {
        self.instances().iter().any(|i| {
            (i.group_path == group_path || i.group_path.starts_with(prefix))
                && (i.worktree_info.as_ref().is_some_and(|wt| wt.managed_by_aoe)
                    || i.workspace_info
                        .as_ref()
                        .is_some_and(|ws| ws.cleanup_on_delete))
        })
    }

    pub(super) fn group_has_containers(&self, group_path: &str, prefix: &str) -> bool {
        self.instances().iter().any(|i| {
            (i.group_path == group_path || i.group_path.starts_with(prefix))
                && i.sandbox_info.as_ref().is_some_and(|s| s.enabled)
        })
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

        // Validate target profile exists when moving across profiles
        if let Some(target) = new_profile {
            if target != ctx.old_profile {
                let profiles = list_profiles()?;
                if !profiles.contains(&target.to_string()) {
                    anyhow::bail!("Profile '{}' does not exist", target);
                }
            }
        }

        let old_prefix = format!("{}/", ctx.old_path);

        // Collect sessions belonging to this group and its descendants
        let affected_ids: Vec<String> = self
            .instances
            .iter()
            .filter(|i| {
                (i.group_path == ctx.old_path || i.group_path.starts_with(&old_prefix))
                    && i.source_profile == ctx.old_profile
            })
            .map(|i| i.id.clone())
            .collect();

        // Update group_path (and optionally source_profile) for all affected sessions
        for id in &affected_ids {
            let new_group_path = if new_path != ctx.old_path {
                let inst = self.get_instance(id);
                match inst {
                    Some(i) if i.group_path == ctx.old_path => new_path.to_string(),
                    Some(i) => format!("{}{}", new_path, &i.group_path[ctx.old_path.len()..]),
                    None => continue,
                }
            } else {
                match self.get_instance(id) {
                    Some(i) => i.group_path.clone(),
                    None => continue,
                }
            };

            if let Some(tp) = new_profile {
                self.move_to_profile(id, tp, new_group_path.clone())?;
            } else {
                self.apply_user_action(id, |inst| {
                    inst.group_path = new_group_path.clone();
                })?;
            }
        }

        // Ensure target profile storage exists when moving across profiles
        if let Some(tp) = new_profile {
            if tp != ctx.old_profile && !self.storages.contains_key(tp) {
                self.storages
                    .insert(tp.to_string(), Storage::new(tp, self.file_watch.clone())?);
            }
        }

        let path_changed = new_path != ctx.old_path;
        let profile_changed = new_profile.is_some_and(|p| p != ctx.old_profile);

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
        let snapshot = self
            .get_instance(&id)
            .map(|i| (i.worktree_info.clone(), i.status, i.project_path.clone()));
        let Some((worktree_info, status, project_path)) = snapshot else {
            anyhow::bail!("Session not found");
        };
        let Some(worktree_info) = worktree_info else {
            anyhow::bail!("Session does not use a worktree");
        };
        if status.blocks_worktree_edit() {
            anyhow::bail!("Stop the session before editing its workdir name");
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

        self.apply_user_action(&id, |inst| {
            inst.project_path = new_path.clone();
            if let Some(branch) = &new_branch {
                if let Some(wt) = inst.worktree_info.as_mut() {
                    wt.branch = branch.clone();
                }
            }
        })?;

        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;
        Ok(())
    }

    pub(super) fn rename_selected(
        &mut self,
        new_title: &str,
        new_group: Option<&str>,
        new_profile: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            // Get current values for comparison
            let (current_title, current_group) = self
                .get_instance(&id)
                .map(|i| (i.title.clone(), i.group_path.clone()))
                .unwrap_or_default();

            // Determine effective title (keep current if empty)
            let effective_title = if new_title.is_empty() {
                current_title.clone()
            } else {
                new_title.to_string()
            };

            // Determine effective group
            let effective_group = match new_group {
                None => current_group.clone(), // Keep current
                Some(g) => g.to_string(),      // Set new (empty string means ungroup)
            };

            // Tied mode (#1927): a worktree session's directory leaf follows
            // its title, so move the directory in lockstep before persisting
            // the new title. The move is gated on a stopped session; a running
            // session surfaces a warning and nothing is renamed. Applied below
            // in both the profile-move and the standard persist paths.
            let mut new_path: Option<String> = None;
            if current_title != effective_title && self.tie_workdir_applies_for(&id) {
                let snapshot = self
                    .get_instance(&id)
                    .map(|i| (i.worktree_info.clone(), i.status, i.project_path.clone()));
                if let Some((Some(worktree_info), status, project_path)) = snapshot {
                    if status.blocks_worktree_edit() {
                        self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                            "Stop the Session to Rename",
                            "This worktree session's directory moves to match the new name, which can't happen while it's running. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
                        ));
                        return Ok(());
                    }
                    let leaf =
                        crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
                    match crate::session::worktree_edit::edit_worktree_workdir(
                        crate::session::worktree_edit::WorktreeEditRequest {
                            worktree_info: &worktree_info,
                            current_path: std::path::Path::new(&project_path),
                            new_name: &leaf,
                            rename_branch: false,
                        },
                    ) {
                        Ok(outcome) => {
                            new_path = Some(outcome.new_path.to_string_lossy().to_string())
                        }
                        // Leaf maps to the current dir: nothing to move, just
                        // rename the title.
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

            // Handle profile change (move session to different profile)
            if let Some(target_profile) = new_profile {
                let current_profile = self
                    .get_instance(&id)
                    .map(|i| i.source_profile.clone())
                    .unwrap_or_else(|| self.config_profile());
                if target_profile != current_profile {
                    // Validate target profile exists
                    let profiles = list_profiles()?;
                    if !profiles.contains(&target_profile.to_string()) {
                        anyhow::bail!("Profile '{}' does not exist", target_profile);
                    }

                    // Get the instance to move
                    let mut instance = self
                        .instances()
                        .iter()
                        .find(|i| i.id == id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;

                    // Apply title and group changes to the instance
                    instance.title = effective_title.clone();
                    instance.group_path = effective_group.clone();

                    // Handle tmux rename if title changed
                    if let Some(orig_inst) = self.get_instance(&id) {
                        if orig_inst.title != effective_title {
                            let tmux_session = orig_inst.tmux_session()?;
                            if tmux_session.exists() {
                                let new_tmux_name =
                                    crate::tmux::Session::generate_name(&id, &effective_title);
                                if let Err(e) = tmux_session.rename(&new_tmux_name) {
                                    tracing::warn!(target: "tui.home", "Failed to rename tmux session: {}", e);
                                } else {
                                    crate::tmux::refresh_session_cache();
                                }
                            }
                        }
                    }

                    // Ensure target profile storage exists
                    if !self.storages.contains_key(target_profile) {
                        self.storages.insert(
                            target_profile.to_string(),
                            Storage::new(target_profile, self.file_watch.clone())?,
                        );
                    }

                    // Update source_profile and save (handles moving between profiles)
                    instance.source_profile = target_profile.to_string();
                    let new_title = instance.title.clone();
                    let moved_path = new_path.clone();
                    self.move_to_profile(&id, target_profile, instance.group_path.clone())?;
                    // apply_user_action (not mutate_instance + save) so a tied
                    // worktree's moved project_path actually persists; save()
                    // via merge_from_tui does not write project_path. (#1927)
                    self.apply_user_action(&id, |inst| {
                        inst.title = new_title.clone();
                        if let Some(path) = &moved_path {
                            inst.project_path = path.clone();
                        }
                    })?;

                    self.rebuild_group_trees();
                    if !effective_group.is_empty() {
                        // Ensure group tree exists for the target profile
                        if !self.group_trees.contains_key(target_profile) {
                            self.group_trees.insert(
                                target_profile.to_string(),
                                GroupTree::new_with_groups(&[], &[]),
                            );
                        }
                        if let Some(tree) = self.group_trees.get_mut(target_profile) {
                            tree.create_group(&effective_group);
                        }
                    }
                    self.save()?;
                    self.reload()?;
                    return Ok(());
                }
            }

            // Rename tmux session BEFORE mutating the instance, so we can
            // look up the session by its current (old) name.
            if current_title != effective_title {
                let old_tmux_session = crate::tmux::Session::new(&id, &current_title)?;
                if old_tmux_session.exists() {
                    let new_tmux_name = crate::tmux::Session::generate_name(&id, &effective_title);
                    if let Err(e) = old_tmux_session.rename(&new_tmux_name) {
                        tracing::warn!(target: "tui.home", "Failed to rename tmux session: {}", e);
                    } else {
                        crate::tmux::refresh_session_cache();
                    }
                }
            }

            self.apply_user_action(&id, |inst| {
                inst.title = effective_title.clone();
                inst.group_path = effective_group.clone();
                if let Some(path) = &new_path {
                    inst.project_path = path.clone();
                }
            })?;

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
        }
        Ok(())
    }

    /// Handle the snooze keybind on the cursor's session. If already snoozed,
    /// wake it immediately (no picker, the user just wants it back).
    /// Otherwise open the duration picker (`SnoozeDurationDialog`) so they
    /// can choose a duration before the row sinks. The actual snooze runs in
    /// `snooze_session_for` once the dialog submits.
    ///
    /// Snooze semantics: a temporary archive that sets `snoozed_until = now +
    /// minutes`, the row sinks to tier 99 alongside archived rows, renders
    /// italic+dim with a `z ` prefix and remaining time in the age column,
    /// and wakes back up automatically when the timer elapses (lazy, no
    /// background task). Duration is resolved at snooze time; changing the
    /// config default does NOT extend in flight snoozes.
    pub(super) fn toggle_snooze_at_cursor(&mut self) -> anyhow::Result<Option<String>> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(None);
        };
        let (is_snoozed, title) = {
            let inst = self.instances.iter().find(|i| i.id == id);
            match inst {
                Some(i) => (i.is_snoozed(), i.title.clone()),
                None => return Ok(None),
            }
        };
        if is_snoozed {
            self.apply_user_action(&id, |inst| inst.unsnooze())?;
            self.flat_items = self.build_flat_items();
            return Ok(Some(format!("Woke: {}", title)));
        }

        self.pending_snooze_session = Some(id);
        self.snooze_duration_dialog = Some(crate::tui::dialogs::SnoozeDurationDialog::new(&title));
        Ok(None)
    }

    /// Apply a snooze with an explicit duration. Called by the duration
    /// picker on submit; also the single place that actually mutates
    /// `snoozed_until` from the TUI. After sinking the row in the Attention
    /// sort, jump to the next needs attention item so the user can keep
    /// triaging.
    pub(super) fn snooze_session_for(
        &mut self,
        id: &str,
        minutes: u32,
    ) -> anyhow::Result<Option<String>> {
        let title = self
            .instance_map
            .get(id)
            .map(|i| i.title.clone())
            .unwrap_or_default();
        self.apply_user_action(id, |inst| inst.snooze(minutes))?;
        self.flat_items = self.build_flat_items();
        if self.sort_order == crate::session::config::SortOrder::Attention {
            self.select_top_attention(None);
        }
        Ok(Some(format!(
            "Snoozed for {}: {}",
            humanize_minutes(minutes),
            title
        )))
    }

    /// Toggle the favorite flag on the cursor's session. Favorited rows
    /// pin above non-favorited peers within the same status tier in the
    /// Attention sort, and render with bold + underline plus a leading
    /// `* ` glyph (see `render.rs`).
    ///
    /// Favorite is orthogonal to archive and snooze: it survives an
    /// unsnooze (the star is the user's persistent "care more" signal),
    /// but archiving clears it because archive is the strongest dismiss
    /// signal and a stale star on a buried row is just visual noise.
    /// Mutual exclusion lives in `Instance::archive()`, not here.
    pub(super) fn toggle_favorite_at_cursor(&mut self) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let is_fav = match self.instances.iter().find(|i| i.id == id) {
            Some(i) => i.is_favorited(),
            None => return Ok(()),
        };
        if is_fav {
            self.apply_user_action(&id, |inst| inst.unfavorite())?;
        } else {
            self.apply_user_action(&id, |inst| inst.favorite())?;
        }
        self.flat_items = self.build_flat_items();
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
            let inst = self.instances.iter().find(|i| &i.id == id)?;
            (!inst.is_archived()).then(|| id.clone())
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

    /// Handle the archive keybind on the cursor's session. Symmetric toggle:
    /// archive an active row, unarchive an archived one. Killing the tmux
    /// pane on archive matches the CLI semantics (archived means "stop
    /// spending CPU on this") so a stale spinner can't keep advertising the
    /// session as alive. Unarchive does NOT respawn the pane; the user
    /// restarts explicitly if they want it back.
    ///
    /// Mirrors `toggle_snooze_at_cursor` but with no picker: archive is
    /// indefinite, so there's nothing to ask the user before sinking the
    /// row. The session reappears at its real tier on unarchive or when
    /// the user sends a message (auto unarchive in `Instance::message_sent`).
    pub(super) fn toggle_archive_at_cursor(&mut self) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let is_archived = match self.instances.iter().find(|i| i.id == id) {
            Some(i) => i.is_archived(),
            None => return Ok(()),
        };
        if is_archived {
            self.apply_user_action(&id, |inst| inst.unarchive())?;
            self.flat_items = self.build_flat_items();
            // Re-seat the cursor on the just-unarchived session. After the
            // flat_items rebuild the row jumps from tier 99 to its real
            // tier, so without this the cursor stays at the old index and
            // ends up on whatever row slid into that slot. The session stays
            // Stopped (archive killed its pane); the user restarts it with `e`
            // when they want it back, same as any other stopped session.
            self.select_session_by_id(&id);
            return Ok(());
        }

        // Kill the pane before flipping the archived bit. If the kill fails
        // (tmux gone, pane already dead) we still archive: the row should
        // sink regardless, since the user explicitly asked for it.
        if let Some(inst) = self.instances.iter().find(|i| i.id == id) {
            if let Err(e) = inst.kill() {
                tracing::warn!("toggle_archive_at_cursor: kill failed (continuing): {}", e);
            }
        }

        // Decide where the cursor lands BEFORE the row sinks, against the
        // pre-archive list the user is actually looking at. Only the
        // non-Attention branch consumes it; Attention re-picks from the top.
        let successor = (self.sort_order != crate::session::config::SortOrder::Attention)
            .then(|| self.archive_successor_session(&id))
            .flatten();

        self.apply_user_action(&id, |inst| inst.archive())?;
        if self.sort_order == crate::session::config::SortOrder::Attention {
            // Attention sort is a triage flow: archiving sinks the row and the
            // cursor advances to the next item that needs attention. That path
            // already lands selection on a live row, so it never showed the
            // dead-pane/selection-swap jank the default sort did.
            self.flat_items = self.build_flat_items();
            self.select_top_attention(None);
            // select_top_attention is a no-op when no session row is visible
            // (the archived row sank into a collapsed Archived section and
            // nothing else is left), which would strand `selected_session`
            // on the now-invisible archived row and leave the cursor index
            // past the shrunken list. Clamp and re-resolve, mirroring the
            // non-Attention fallback below.
            if self.selected_session.as_deref() == Some(id.as_str()) {
                self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
                self.update_selected();
            }
        } else {
            // Advance to the next session instead of following the archived
            // row into the Archived section: archiving reads as "I'm done
            // with this one", so the cursor stays up in the active list and
            // moves on. The preview retargets on its own: `render_preview`
            // re-derives the capture target from `selected_session` every
            // frame, the cache gates on a session-id mismatch, and the
            // capture worker drops stale frames on retarget, so the pane
            // tracks the new selection without the dead-pane flash that
            // motivated the old follow-the-row behavior (#2025). The
            // Archived section is not auto-revealed; its header already
            // shows the updated count as feedback.
            self.flat_items = self.build_flat_items();
            match successor {
                Some(next) => self.select_session_by_id(&next),
                None => {
                    // No other active session: clamp and let
                    // `update_selected` resolve whatever sits at the cursor
                    // now (typically the Archived section header).
                    self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
                    self.update_selected();
                }
            }
        }
        Ok(())
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
                .iter()
                .filter(|i| !i.is_archived())
                .filter(|i| {
                    self.active_profile
                        .as_ref()
                        .is_none_or(|p| &i.source_profile == p)
                })
                .filter(|i| super::project_group_name(i) == group_path)
                .map(|i| i.id.clone())
                .collect(),
            // Manual groups can nest, so a session belongs when its path
            // matches exactly or sits beneath the group. Scope to the group's
            // owning profile the same way `delete_selected_group` does.
            crate::session::config::GroupByMode::Manual => {
                let prefix = format!("{}/", group_path);
                self.instances
                    .iter()
                    .filter(|i| !i.is_archived())
                    .filter(|i| i.group_path == group_path || i.group_path.starts_with(&prefix))
                    .filter(|i| {
                        self.selected_group_profile
                            .as_ref()
                            .is_none_or(|p| p == &i.source_profile)
                    })
                    .map(|i| i.id.clone())
                    .collect()
            }
        }
    }

    /// Archive every active session under the selected group. Mirrors the
    /// single-row archive path in `toggle_archive_at_cursor`: kill each pane
    /// before flipping the archived bit, then reveal the Archived section so
    /// the rows stay visible. Triggered behind a confirmation prompt, so there
    /// is no further guard here.
    pub(super) fn archive_selected_group(&mut self) -> anyhow::Result<()> {
        let ids = self.active_sessions_in_selected_group();
        if ids.is_empty() {
            return Ok(());
        }
        for inst in self.instances.iter().filter(|i| ids.contains(&i.id)) {
            if let Err(e) = inst.kill() {
                tracing::warn!("archive_selected_group: kill failed (continuing): {}", e);
            }
        }
        self.bulk_apply_user_action(&ids, |inst| inst.archive())?;
        self.reveal_archived_section();
        self.flat_items = self.build_flat_items();
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

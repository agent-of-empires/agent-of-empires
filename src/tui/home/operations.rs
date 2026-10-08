//! Session operations for HomeView (create, delete, rename)

use crate::session::{Instance, Item, LifecycleOperation, StartBlocked, Status};
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

impl HomeView {
    /// Why this process may not write `projects.json`, or `None` when it may.
    ///
    /// The runtime republishes the registry in every snapshot
    /// (`global_projects`), so once it owns the rows a local write diverges
    /// from the state it will send back next. No runtime route exists for a
    /// client-side pin: the daemon's own project mutations map to
    /// `POST /api/projects` / `PATCH /api/projects/{name}` /
    /// Project registry has an independent local-write policy; sessions/groups never write locally.
    fn project_write_block(&self) -> Option<&'static str> {
        if !self.project_registry_authoritative {
            return None;
        }
        if self.session_feed.cityhall_mode() {
            return Some("The attached runtime serves a City Hall client, whose policy does not allow this process to write the project registry locally");
        }
        if self.sidebar_source != crate::tui::session_feed::SidebarSource::Daemon
            || !self.session_feed.mutations_available()
        {
            return Some("The runtime is read-only or unreachable, so this process may not write the project registry locally");
        }
        None
    }

    fn refuse_project_write(&mut self, reason: &'static str) {
        tracing::info!(target: "tui.home", reason, "Refused a local project registry write");
        self.info_dialog = Some(InfoDialog::new("Read-only", reason));
    }

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
        if let Some(reason) = self.project_write_block() {
            // Before the header is even read: every branch below writes the
            // registry, and none of it can be unwound once started.
            self.refuse_project_write(reason);
            return;
        }
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

        if self.refuse_start_if_shelved(&id) {
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
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        if self.restart_in_flight.contains(&id) {
            self.info_dialog = Some(InfoDialog::new(
                "Restart in progress",
                "This session is still restarting. Wait for it to finish before deleting.",
            ));
            return Ok(());
        }
        self.session_feed.submit_request(
            id,
            crate::tui::session_feed::SessionRequest::Purge(crate::daemon::DeleteSessionBody {
                delete_worktree: options.delete_worktree,
                delete_branch: options.delete_branch,
                delete_sandbox: options.delete_sandbox,
                force_delete: options.force_delete,
                keep_scratch: options.keep_scratch,
                ..Default::default()
            }),
        )
    }

    pub(super) fn delete_selected_group(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.selected_group.clone() else {
            return Ok(());
        };
        let group = self.canonical_group_location(&path)?;
        let prefix = format!("{path}/");
        let has_members =
            self.instances()
                .any(group_membership(&path, &prefix, Some(&group.profile)));
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::DeleteGroup(crate::daemon::DeleteGroupBody {
                group,
                mode: if has_members {
                    crate::daemon::DeleteGroupMode::KeepSessions
                } else {
                    crate::daemon::DeleteGroupMode::EmptyOnly
                },
                cleanup: Default::default(),
            }),
            super::NamespaceIntent::Plain,
        )
    }

    pub(super) fn delete_group_with_sessions(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<()> {
        let Some(path) = self.selected_group.clone() else {
            return Ok(());
        };
        let group = self.canonical_group_location(&path)?;
        let prefix = format!("{path}/");
        let (has_creating, has_restarting) = {
            let is_member = group_membership(&path, &prefix, Some(&group.profile));
            let mut has_creating = false;
            let mut has_restarting = false;
            for row in self.instances().filter(|row| is_member(row)) {
                has_creating |= row.status == Status::Creating;
                has_restarting |= self.restart_in_flight.contains(&row.id);
            }
            (has_creating, has_restarting)
        };
        anyhow::ensure!(!has_creating, "A session in this group is still being created. Wait for it to finish before deleting the group.");
        anyhow::ensure!(!has_restarting, "A session in this group is still restarting. Wait for it to finish before deleting the group.");
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::DeleteGroup(crate::daemon::DeleteGroupBody {
                group,
                mode: crate::daemon::DeleteGroupMode::DeleteSessions,
                cleanup: crate::daemon::DeleteSessionBody {
                    delete_worktree: options.delete_worktrees,
                    delete_branch: options.delete_branches,
                    delete_sandbox: options.delete_containers,
                    force_delete: options.force_delete_worktrees,
                    keep_scratch: false,
                    ..Default::default()
                },
            }),
            super::NamespaceIntent::Plain,
        )
    }

    /// Forget a stuck deletion record; finish runtime cleanup off the input thread.
    pub(super) fn force_remove_session(
        &mut self,
        session_id: &str,
        expected_generation: std::num::NonZeroU64,
    ) -> anyhow::Result<()> {
        let row = self
            .session_feed
            .applied_session(session_id)
            .ok_or_else(|| anyhow::anyhow!("No canonical purge owner is available"))?;
        anyhow::ensure!(
            row.lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.op == LifecycleOperation::Purge
                    && reservation.generation == expected_generation.get()),
            "The purge owner changed. Review current canonical state and reopen Force Remove."
        );
        self.session_feed.submit(
            session_id.to_owned(),
            crate::daemon::SessionMutation::AbandonPurge(crate::daemon::AbandonPurgeBody {
                expected_generation,
            }),
        )?;
        self.flash_status(
            "Forgetting the purge record; runtime cleanup will be scheduled by the daemon",
        );
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
        let Some(ctx) = self.group_rename_context.as_ref() else {
            return Ok(());
        };
        let path = new_group
            .filter(|path| !path.is_empty())
            .unwrap_or(&ctx.old_path);
        let profile = new_profile.unwrap_or(&ctx.old_profile);
        if path == ctx.old_path && profile == ctx.old_profile {
            return Ok(());
        }
        let body = crate::daemon::MoveGroupBody {
            source: crate::daemon::GroupLocation {
                profile: ctx.old_profile.clone(),
                path: ctx.old_path.clone(),
            },
            target: crate::daemon::GroupLocation {
                profile: profile.to_owned(),
                path: path.to_owned(),
            },
        };
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::MoveGroup(body),
            super::NamespaceIntent::Plain,
        )?;
        self.group_rename_context = None;
        Ok(())
    }

    /// Queue an explicit workdir/optional branch edit; the daemon owns filesystem and metadata.
    pub(super) fn set_worktree_name_for_selected(
        &mut self,
        new_name: &str,
        rename_branch: bool,
    ) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        self.session_feed.submit_request(
            id,
            crate::tui::session_feed::SessionRequest::SetWorktreeName(
                crate::daemon::SetWorktreeNameBody {
                    name: new_name.to_owned(),
                    rename_branch,
                },
            ),
        )
    }

    /// Queue one typed attachment RPC; daemon owns worktree rollback, worker restart and warnings.
    pub(super) fn add_project_to_session(
        &mut self,
        id: &str,
        repo_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let instance = self
            .get_instance(id)
            .ok_or_else(|| anyhow::anyhow!("Session no longer exists"))?;
        anyhow::ensure!(
            !matches!(instance.status, Status::Creating | Status::Deleting),
            "Wait for the session to finish starting or deleting before attaching a project"
        );
        anyhow::ensure!(!instance.status.blocks_worktree_edit(), "The agent is mid-turn and attaching restarts it; wait for the turn to finish or stop the session first");
        anyhow::ensure!(
            !instance.is_trashed(),
            "This session is in the trash; restore it before attaching a project"
        );
        anyhow::ensure!(!instance.is_archived(), "This session is archived and its agent stays stopped; unarchive it before attaching a project");
        let project = repo_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("The project path is not valid UTF-8"))?
            .to_owned();
        self.session_feed.submit_request(
            id.to_owned(),
            crate::tui::session_feed::SessionRequest::AttachProject(
                crate::daemon::AttachProjectBody {
                    project,
                    attach_existing_branch: false,
                },
            ),
        )
    }

    pub(super) fn rename_selected(
        &mut self,
        new_title: &str,
        new_group: Option<&str>,
        new_profile: Option<&str>,
        rename_branch: bool,
    ) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        self.session_feed.submit_request(
            id,
            crate::tui::session_feed::SessionRequest::Rename(crate::daemon::RenameSessionBody {
                title: (!new_title.is_empty()).then(|| new_title.to_owned()),
                group: new_group.map(str::to_owned),
                profile: new_profile.map(str::to_owned),
                rename_branch,
            }),
        )
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
        // A trashed row cannot be meaningfully archived, so `z` on it restores the
        // session from the trash instead. See #2489.
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
    /// The daemon commits the trash reservation, teardown and relocation; UI follows its receipt fence.
    pub(super) fn trash_session_by_id(&mut self, id: &str) {
        if let Err(error) = self.session_feed.submit_request(
            id.to_owned(),
            crate::tui::session_feed::SessionRequest::Trash(crate::daemon::TrashSessionBody {
                kill_pane: true,
            }),
        ) {
            self.info_dialog = Some(InfoDialog::new(
                "Could Not Trash Session",
                &error.to_string(),
            ));
        }
    }

    /// Restore the selected trashed session, clearing `trashed_at` so it
    /// returns to its prior bucket. No-op when the selection is not trashed.
    /// The session stays stopped (trash killed its panes); the user restarts
    /// it with `e` like any stopped session. See #2489.
    pub(super) fn restore_selected_from_trash(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        if !self.instances.get(&id).is_some_and(|row| row.is_trashed()) {
            return;
        }
        if let Err(error) = self
            .session_feed
            .submit_request(id, crate::tui::session_feed::SessionRequest::Restore)
        {
            self.info_dialog = Some(InfoDialog::new("Restore Failed", &error.to_string()));
        }
    }

    /// Restore every trashed session back into its group. The synthetic Trash
    /// section's "Restore All" bulk action: drives each row through the same
    /// per-row `restore_selected_from_trash` (claim, off-lock worktree move,
    /// untrash) so the claim/commit races (#2541) are handled identically to a
    /// single restore. Each row's failure surfaces its own info dialog; the
    /// last one wins, which is acceptable for a rare bulk recovery.
    pub(super) fn restore_all_from_trash(&mut self) {
        let mut ids: Vec<_> = self
            .instances()
            .filter(|row| {
                row.is_trashed()
                    && self
                        .active_profile
                        .as_ref()
                        .is_none_or(|profile| *profile == row.source_profile)
            })
            .map(|row| row.id.clone())
            .collect();
        ids.sort();
        if ids.is_empty() {
            return;
        }
        if let Err(error) = self.session_feed.submit_batch(
            ids.into_iter()
                .map(|id| (id, crate::tui::session_feed::SessionRequest::Restore)),
        ) {
            self.info_dialog = Some(InfoDialog::new("Restore Failed", &error.to_string()));
        }
    }

    /// Restore every archived row through the daemon. The section updates from
    /// receipts and canonical snapshots, not from local storage writes.
    pub(super) fn unarchive_all(&mut self) {
        let requests = self
            .instances
            .values()
            .filter(|row| row.is_archived() && !row.is_trashed())
            .map(|row| {
                (
                    row.id.clone(),
                    crate::tui::session_feed::SessionRequest::Mutation(
                        crate::daemon::SessionMutation::Archive(crate::daemon::UpdateArchiveBody {
                            archived: false,
                            kill_pane: true,
                        }),
                    ),
                )
            });
        if let Err(error) = self.session_feed.submit_batch(requests) {
            self.info_dialog = Some(InfoDialog::new("Restore Failed", &error.to_string()));
        }
    }

    /// Purge the current view's trashed ids using each authoritative row's cleanup defaults.
    /// Lifecycle claims, resource teardown and receipts are daemon-owned.
    pub(super) fn empty_trash_all(&mut self) {
        let mut ids: Vec<_> = self
            .instances()
            .filter(|row| {
                row.is_trashed()
                    && self
                        .active_profile
                        .as_ref()
                        .is_none_or(|profile| *profile == row.source_profile)
            })
            .map(|row| row.id.clone())
            .collect();
        ids.sort();
        if ids.is_empty() {
            return;
        }
        let requests = ids.into_iter().map(|id| {
            (
                id,
                crate::tui::session_feed::SessionRequest::Purge(crate::daemon::DeleteSessionBody {
                    expected_trash: true,
                    use_cleanup_defaults: true,
                    force_delete: true,
                    keep_scratch: false,
                    ..Default::default()
                }),
            )
        });
        if let Err(error) = self.session_feed.submit_batch(requests) {
            self.info_dialog = Some(InfoDialog::new("Could Not Empty Trash", &error.to_string()));
        }
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

    /// Queue the whole group's archive before submitting any row. The daemon
    /// tears down each pane and publishes its committed state.
    pub(super) fn archive_selected_group(&mut self) -> anyhow::Result<()> {
        let ids = self.active_sessions_in_selected_group();
        if ids.is_empty() {
            return Ok(());
        }
        self.session_feed.submit_batch(ids.into_iter().map(|id| {
            (
                id,
                crate::tui::session_feed::SessionRequest::Mutation(
                    crate::daemon::SessionMutation::Archive(crate::daemon::UpdateArchiveBody {
                        archived: true,
                        kill_pane: true,
                    }),
                ),
            )
        }))?;
        self.reveal_archived_section();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::home::tests::{native_state, payload_bytes, request_with_headers};
    use std::os::unix::fs::PermissionsExt;

    async fn check_worktree_edit(status: Status, sandboxed: bool, running: bool) {
        let home = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(false);
        crate::session::config::update_config(|config| {
            config.sandbox.container_runtime = crate::session::config::ContainerRuntimeName::Docker;
        })
        .unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls = home.path().join("runtime-calls");
        let running_path = home.path().join("running");
        std::fs::write(&running_path, if running { "true" } else { "false" }).unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("AOE_TEST_RUNTIME_CALLS", calls.as_path()),
            ("AOE_TEST_RUNNING", running_path.as_path()),
        ]);
        let script = bin.join("docker");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AOE_TEST_RUNTIME_CALLS\"\ncase \"$1 $2\" in\n 'container inspect') cat \"$AOE_TEST_RUNNING\";;\n *) exit 0;;\nesac\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _path = crate::session::test_support::path_prepended(&bin);
        let (repo_dir, _repo) = crate::git::test_support::init_repo();
        let worktrees = tempfile::tempdir().unwrap();
        let old_path = worktrees.path().join("old");
        let new_path = worktrees.path().join("new");
        let git = crate::git::GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
        git.create_worktree("old", &old_path, true, None).unwrap();
        git.unlock_worktree(&old_path);
        let mut row = Instance::new("old", old_path.to_str().unwrap());
        row.status = status;
        row.group_path = "work".into();
        row.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "old".into(),
            main_repo_path: repo_dir.path().to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        if sandboxed {
            row.sandbox_info = Some(
                serde_json::from_value(serde_json::json!({
                    "enabled": true, "image": "aoe-test", "container_name": "aoe-test",
                }))
                .unwrap(),
            );
        }
        let id = row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("worktree-edit", vec![row]);
        let state = native_state(&["worktree-edit"]).await;
        let before = payload_bytes("worktree-edit");
        let (response_status, headers, body) = request_with_headers(
            &state,
            "PATCH",
            &format!("/api/sessions/{id}/worktree-name"),
            serde_json::json!({"name":"new", "rename_branch":false}),
        )
        .await;
        if status.blocks_worktree_edit() || (sandboxed && running) {
            assert_eq!(response_status, axum::http::StatusCode::CONFLICT);
            assert_eq!(body["error"], "session_running");
            assert_eq!(payload_bytes("worktree-edit"), before);
            assert!(old_path.exists());
            assert!(!new_path.exists());
            if status.blocks_worktree_edit() {
                assert!(
                    !calls.exists(),
                    "status admission precedes any container side effect"
                );
            } else {
                let calls = std::fs::read_to_string(&calls).unwrap();
                assert!(!calls.lines().any(|line| line.starts_with("rm ")));
            }
        } else {
            assert!(response_status.is_success(), "{response_status}: {body}");
            assert!(headers.contains_key(crate::daemon::RUNTIME_EPOCH_HEADER));
            assert!(!old_path.exists());
            assert!(new_path.join(".git").exists());
            let rows =
                crate::server::test_support::load_instances_from_disk_for_test("worktree-edit");
            let stored = rows.iter().find(|row| row.id == id).unwrap();
            assert_eq!(std::path::Path::new(&stored.project_path), new_path);
            assert_eq!(stored.group_path, "work");
            assert_eq!(stored.worktree_info.as_ref().unwrap().branch, "old");
            if sandboxed {
                let calls = std::fs::read_to_string(&calls).unwrap();
                assert!(calls
                    .lines()
                    .any(|line| line.starts_with("rm ") && !line.contains("--force")));
            } else {
                assert!(!calls.exists());
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn idle_sandbox_with_running_container_blocks() {
        check_worktree_edit(Status::Idle, true, true).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn idle_sandbox_with_stopped_container_is_safe() {
        check_worktree_edit(Status::Idle, true, false).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn worktree_rename_block_checks_status_before_container() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
            Status::Deleting,
        ] {
            check_worktree_edit(status, true, true).await;
            check_worktree_edit(status, false, false).await;
        }
        check_worktree_edit(Status::Idle, false, false).await;
    }
}

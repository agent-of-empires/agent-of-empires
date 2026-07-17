//! Session operations for HomeView (create, delete, rename)

use crate::session::builder::{self, InstanceParams};
use crate::session::{list_profiles, ClaimOp, GroupTree, Instance, Item, Status, Storage};
use crate::tui::deletion_poller::DeletionRequest;
use crate::tui::dialogs::{DeleteOptions, GroupDeleteOptions, InfoDialog, NewSessionData};
use crate::tui::restart_poller::RestartRequest;

use super::HomeView;

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

    pub(super) fn create_session(&mut self, data: NewSessionData) -> anyhow::Result<String> {
        let target_profile = data.profile.clone();

        // In unified mode, all instances are loaded, so use them for title dedup.
        // For the target profile, filter to that profile's instances.
        let existing_titles: Vec<&str> = self
            .instances()
            .filter(|i| i.source_profile == target_profile)
            .map(|i| i.title.as_str())
            .collect();
        let existing_branches: Vec<&str> = self
            .instances()
            .filter(|i| i.source_profile == target_profile)
            .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
            .collect();

        // `structured` is applied post-build (mirrors the web create
        // handler); read it off before the params conversion consumes data.
        let structured = data.structured;
        let params = InstanceParams::from(data);

        let build_result = builder::build_instance(
            params,
            &existing_titles,
            &existing_branches,
            &target_profile,
        )?;
        let mut instance = build_result.instance;
        instance.source_profile = target_profile.clone();
        #[cfg(feature = "serve")]
        if structured {
            builder::structured::apply_structured_choice(&mut instance);
        }
        #[cfg(not(feature = "serve"))]
        let _ = structured;
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
    /// The start cascade itself runs on the `RestartPoller` worker thread (it
    /// shells out to docker and runs the before_start host hook, which can
    /// block for seconds), so the TUI event loop never blocks. The post-cascade
    /// `Instance` (with `restart_with_size`'s mutations: `resume_probe_failed_sid`,
    /// `last_error`, container id, etc.) is written back via
    /// `apply_restart_results`.
    ///
    /// The wake-up message is read from the resolved config
    /// (`session.restart_wake_message`); an empty value disables the
    /// wake-up entirely while still running the restart.
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

        // A restart cascade for this row is already running on the poller
        // worker. The cascade is off the event loop now, so the 1.5s
        // keyboard-repeat debounce below does not cover a deliberate second
        // press during a multi-second pull. Without this guard the worker would
        // enqueue a duplicate request and, running serially, restart the row a
        // second time, tearing down the container the first restart just built.
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
                    || (snoozed && in_attention)
                    || inst.pane_dead_observed;
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
                self.rebuild_flat_items();
            }
        }

        // The start cascade shells out to docker (image pull, container
        // create/start) and runs the before_start host hook, any of which can
        // block for seconds. Running it inline froze the TUI
        // event loop, so mirror the recovery/stop paths: flip the row to
        // Starting for immediate feedback, then run the cascade on the restart
        // poller's worker thread. The post-cascade snapshot (and the wake-up)
        // are handled via `apply_restart_results`.
        let size = crate::terminal::get_size();

        // Status::Starting + a fresh last_start_time keeps the StatusPoller from
        // flipping the row to Error before the worker finishes (the same grace
        // startup recovery relies on); touch bumps the row on the user gesture.
        self.mutate_instance(&id, |inst| {
            inst.status = Status::Starting;
            inst.last_error = None;
            inst.last_start_time = Some(std::time::Instant::now());
            inst.touch_last_accessed();
        });
        self.save()?;

        let Some(instance) = self.get_instance(&id).cloned() else {
            return Ok(());
        };

        // Resolve the wake message on the main thread (config access). Empty is
        // the documented opt-out; the worker skips the wake-up then.
        let wake_message = crate::session::resolve_config(&instance.source_profile)
            .map(|c| c.session.restart_wake_message.clone())
            .unwrap_or_else(|_| "wake up: pick up what you were doing".to_string());

        self.restart_in_flight.insert(id.clone());
        self.restart_poller.request_restart(RestartRequest {
            session_id: id,
            instance,
            size,
            wake_message,
        });
        Ok(())
    }

    pub(super) fn delete_selected(&mut self, options: &DeleteOptions) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            // Refuse to delete a row whose restart cascade is still running on
            // the worker: deletion would fire docker commands against the same
            // container the restart worker is mid-creating, orphaning resources
            // non-deterministically. The old synchronous cascade made this race
            // impossible (the UI thread could not accept a delete mid-restart);
            // off-threading the cascade removed that implicit lock.
            if self.restart_in_flight.contains(&id) {
                self.info_dialog = Some(InfoDialog::new(
                    "Restart in progress",
                    "This session is still restarting. Wait for it to finish before deleting.",
                ));
                return Ok(());
            }

            // #2541: a permanent delete of a trashed session is a purge that can
            // race a restore from another process. Win the Purge claim under the
            // flock before the unlocked teardown; refuse if a peer restore holds
            // a fresh claim. A live-session delete has no restore to race, so it
            // skips the claim.
            let was_trashed = self.get_instance(&id).is_some_and(|i| i.is_trashed());
            if was_trashed {
                match self.claim_trashed_purge(&id, was_trashed) {
                    Ok(crate::session::claim::PurgeClaimDecision::Claimed) => {
                        self.purge_claimed.insert(id.clone());
                    }
                    Ok(crate::session::claim::PurgeClaimDecision::Restored)
                    | Ok(crate::session::claim::PurgeClaimDecision::RestoreInProgress) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Restore in progress",
                            "This session is being restored by another process; it was not deleted.",
                        ));
                        return Ok(());
                    }
                    Ok(crate::session::claim::PurgeClaimDecision::AlreadyGone) => {
                        self.drop_peer_deleted_rows(std::slice::from_ref(&id));
                        self.rebuild_flat_items();
                        return Ok(());
                    }
                    Err(()) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Delete Failed",
                            "Could not claim the delete under the storage lock. Try again.",
                        ));
                        return Ok(());
                    }
                }
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

    /// Decide the Purge claim for a trashed session before its unlocked
    /// teardown, under the storage flock (the cross-process serialization
    /// point). Uses the shared `decide_purge_claim` so the TUI closes the same
    /// window (peer restore un-trashed the row between snapshot and claim)
    /// as the CLI and server. `Err(())` is a storage failure (surfaced as a
    /// generic delete error), kept distinct from a claim decision. See #2541.
    fn claim_trashed_purge(
        &self,
        id: &str,
        was_trashed: bool,
    ) -> Result<crate::session::claim::PurgeClaimDecision, ()> {
        let Some(profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
            return Ok(crate::session::claim::PurgeClaimDecision::AlreadyGone);
        };
        let Some(storage) = self.storages.get(&profile) else {
            tracing::warn!(
                target: "tui.home",
                profile = %profile,
                id = %id,
                "purge claim: no storage registered for profile"
            );
            return Err(());
        };
        storage
            .update(|insts, _groups| {
                Ok(crate::session::claim::decide_purge_claim(
                    insts,
                    id,
                    was_trashed,
                    chrono::Utc::now(),
                ))
            })
            .map_err(|e| {
                tracing::warn!(target: "tui.home", id = %id, "purge claim failed: {e}");
            })
    }

    /// Finalize a claimed trashed-purge under the flock: apply the #2534 recheck
    /// (keep the row if a peer restored it mid-purge) and release the owned
    /// Purge claim, else drop the row. `Ok(true)` = kept (restored mid-purge),
    /// `Ok(false)` = removed, `Err(())` = storage failure (the row is untouched
    /// on disk, so the caller must not treat it as removed). See #2541.
    pub(super) fn finalize_claimed_purge(&mut self, id: &str) -> Result<bool, ()> {
        let Some(profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
            return Ok(false);
        };
        let Some(storage) = self.storages.get(&profile) else {
            return Err(());
        };
        storage
            .update(|insts, _groups| {
                Ok(matches!(
                    crate::session::claim::finalize_purge_removal(insts, id, true),
                    crate::session::claim::PurgeCommit::KeptRestored
                ))
            })
            .map_err(|e| {
                tracing::warn!(target: "tui.home", id = %id, "purge finalize failed: {e}");
            })
    }

    /// Release a trashed-purge claim on a kept row (teardown failed),
    /// ownership-guarded so a peer's fresh Restore claim survives. See #2541.
    pub(super) fn release_trashed_purge_claim(&self, id: &str) {
        let Some(profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
            return;
        };
        if let Some(storage) = self.storages.get(&profile) {
            let _ = storage.update(|insts, _groups| {
                if let Some(stored) = insts.iter_mut().find(|i| i.id == id) {
                    stored.clear_op_claim_if_owned(ClaimOp::Purge);
                }
                Ok(())
            });
        }
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

    pub(super) fn delete_group_with_sessions(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<()> {
        if let Some(group_path) = self.selected_group.take() {
            let owning_profile = self.selected_group_profile.take();
            let prefix = format!("{}/", group_path);

            // Scoped so the borrow of `group_path` / `owning_profile` ends
            // before the restart-in-flight bail-out moves them back into self.
            let sessions_to_delete: Vec<String> = {
                let is_member = group_membership(&group_path, &prefix, owning_profile.as_deref());
                self.instances()
                    .filter(|i| is_member(i))
                    .map(|i| i.id.clone())
                    .collect()
            };

            // Refuse the whole group delete if any member is mid-restart (same
            // concurrent-docker race as delete_selected). Restore the selection
            // we `take()`'d above so the group stays put.
            if sessions_to_delete
                .iter()
                .any(|sid| self.restart_in_flight.contains(sid))
            {
                self.selected_group = Some(group_path);
                self.selected_group_profile = owning_profile;
                self.info_dialog = Some(InfoDialog::new(
                    "Restart in progress",
                    "A session in this group is still restarting. Wait for it to finish before deleting the group.",
                ));
                return Ok(());
            }

            self.bulk_apply_user_action(&sessions_to_delete, |inst| {
                inst.status = Status::Deleting;
                inst.group_path = String::new();
            })?;

            for session_id in &sessions_to_delete {
                if let Some(inst) = self.get_instance(session_id) {
                    let delete_worktree =
                        options.delete_worktrees && inst.has_managed_worktree_or_workspace();
                    let delete_branch =
                        options.delete_branches && inst.has_managed_worktree_or_workspace();
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
            self.rebuild_flat_items();
        }
        Ok(())
    }

    /// Force-remove a session from storage. Worktree and branch cleanup are
    /// skipped (the original deletion already attempted them), but the sandbox
    /// container IS torn down best-effort so a stuck delete cannot orphan a
    /// live container. Both run off-thread so a hung tmux or docker call cannot
    /// block the storage update on the TUI input thread. Used for sessions
    /// stuck in the Deleting state where the background deletion thread never
    /// returned a result.
    pub(super) fn force_remove_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        if let Some(inst) = self.instances.get(session_id) {
            let inst = inst.clone();
            std::thread::spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inst.kill_all_tmux_sessions()
                })) {
                    tracing::error!(
                        target: "session.delete",
                        session_id = %inst.id,
                        "force_remove tmux teardown panicked: {:?}",
                        panic
                    );
                }
                if inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) {
                    let container = crate::containers::DockerContainer::from_session_id(&inst.id);
                    if let crate::containers::Teardown::Failed(e) = container.teardown(&inst.id) {
                        tracing::warn!(
                            target: "session.delete",
                            session_id = %inst.id,
                            "force_remove container teardown failed: {}",
                            e
                        );
                    }
                }
            });
        }
        self.remove_instance(session_id);
        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;
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
        let is_member = group_membership(&ctx.old_path, &old_prefix, Some(&ctx.old_profile));
        let affected_ids: Vec<String> = self
            .instances
            .values()
            .filter(|i| is_member(i))
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
        let snapshot = self.get_instance(&id).map(|i| {
            (
                i.worktree_info.clone(),
                i.status,
                i.project_path.clone(),
                i.is_sandboxed(),
            )
        });
        let Some((worktree_info, status, project_path, is_sandboxed)) = snapshot else {
            anyhow::bail!("Session not found");
        };
        let Some(worktree_info) = worktree_info else {
            anyhow::bail!("Session does not use a worktree");
        };
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
        if crate::session::worktree_edit::sandbox_container_holds_worktree(&id, is_sandboxed) {
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
        rename_branch: bool,
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
            let mut new_branch: Option<String> = None;
            // Fire when the title changed (dir follows it) OR the user opted to
            // rename the branch (which may be requested even with the title
            // unchanged, to bring a drifted branch back in line with the dir).
            if (current_title != effective_title || rename_branch)
                && self.tie_workdir_applies_for(&id)
            {
                let snapshot = self.get_instance(&id).map(|i| {
                    (
                        i.worktree_info.clone(),
                        i.status,
                        i.project_path.clone(),
                        i.is_sandboxed(),
                    )
                });
                if let Some((Some(worktree_info), status, project_path, is_sandboxed)) = snapshot {
                    // A sandbox session keeps its container alive (running
                    // `sleep infinity`) even while the agent is Idle, and that
                    // container bind-mounts the worktree directory. The move
                    // below `git worktree move`s that dir, which the kernel
                    // refuses while it is an active mount source (EBUSY ->
                    // "fatal: failed to move"). Stopping the session tears the
                    // container down and releases the mount. We only inspect
                    // the container when the status check hasn't already
                    // blocked, so the common non-sandbox path spawns no
                    // `docker inspect`. See #1927 follow-up.
                    let container_running = !status.blocks_worktree_edit()
                        && crate::session::worktree_edit::sandbox_container_holds_worktree(
                            &id,
                            is_sandboxed,
                        );
                    if let Some(reason) =
                        worktree_rename_block(status, is_sandboxed, container_running)
                    {
                        let body = match reason {
                            WorktreeRenameBlock::ActiveAgent => "This worktree session's directory moves to match the new name, which can't happen while it's running. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
                            WorktreeRenameBlock::SandboxContainer => "This sandbox session's container is mounting the worktree directory, so it can't be moved to match the new name. Stop the session first, or disable \"Tie Worktree Directory to Session Name\" to relabel it freely.",
                        };
                        self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                            "Stop the Session to Rename",
                            body,
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
                            rename_branch,
                        },
                    ) {
                        Ok(outcome) => {
                            // Discard the stale container only when the dir
                            // actually moved. A branch-only rename (title
                            // unchanged, toggle armed) leaves the path, and thus
                            // the mount and working dir, valid, so there is
                            // nothing stale to recreate.
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
                        // Leaf maps to the current dir and no branch rename was
                        // requested: nothing to move, just rename the title.
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
                        .get_instance(&id)
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
                    let moved_branch = new_branch.clone();
                    self.move_to_profile(&id, target_profile, instance.group_path.clone())?;
                    // apply_user_action (not mutate_instance + save) so a tied
                    // worktree's moved project_path actually persists; save()
                    // via merge_from_tui does not write project_path. (#1927)
                    self.apply_user_action(&id, |inst| {
                        inst.title = new_title.clone();
                        if let Some(path) = &moved_path {
                            inst.project_path = path.clone();
                        }
                        if let Some(branch) = &moved_branch {
                            if let Some(wt) = inst.worktree_info.as_mut() {
                                wt.branch = branch.clone();
                            }
                        }
                    })?;

                    // Drop the source profile's now-empty copy of the group so
                    // it does not linger as a duplicate header alongside the
                    // target profile's copy in unified view. `current_group` is
                    // the session's pre-move path; the restart-with-edits path
                    // does the same after its own `move_to_profile`.
                    self.prune_empty_group(&current_profile, &current_group);

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
                if let Some(branch) = &new_branch {
                    if let Some(wt) = inst.worktree_info.as_mut() {
                        wt.branch = branch.clone();
                    }
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
            let inst = self.instances.get(&id);
            match inst {
                Some(i) => (i.is_snoozed(), i.title.clone()),
                None => return Ok(None),
            }
        };
        if is_snoozed {
            self.apply_user_action(&id, |inst| inst.unsnooze())?;
            self.rebuild_flat_items();
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
            .instances
            .get(id)
            .map(|i| i.title.clone())
            .unwrap_or_default();
        self.apply_user_action(id, |inst| inst.snooze(minutes))?;
        self.rebuild_flat_items();
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
        let is_fav = match self.instances.get(&id) {
            Some(i) => i.is_favorited(),
            None => return Ok(()),
        };
        if is_fav {
            self.apply_user_action(&id, |inst| inst.unfavorite())?;
        } else {
            self.apply_user_action(&id, |inst| inst.favorite())?;
        }
        self.rebuild_flat_items();
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

    /// Manual unread toggle (`U`). Symmetric: a read row becomes unread (put
    /// it back in the attention queue), an unread row becomes read. The row's
    /// `theme.unread` color is the feedback, so there is no toast. No-op when
    /// the feature is disabled.
    pub(super) fn toggle_unread_at_cursor(&mut self) -> anyhow::Result<()> {
        if !crate::session::unread_enabled() {
            return Ok(());
        }
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        if !self.instances.contains_key(&id) {
            return Ok(());
        }
        self.apply_user_action(&id, |inst| inst.toggle_unread())?;
        // Hold this row for the current visit so the dwell doesn't undo a fresh
        // `u` while the cursor stays on it; the hold is released once the cursor
        // leaves (see `tick_unread_dwell`). Toggling back to read drops it.
        if self.get_instance(&id).is_some_and(|i| i.is_unread()) {
            self.manual_unread_hold = Some(id.clone());
        } else if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            self.manual_unread_hold = None;
        }
        self.rebuild_flat_items();
        // In Attention sort, toggling unread changes the row's rank, so the
        // rebuild can move it; reseat the cursor by id so the next action
        // still targets this session.
        self.select_session_by_id(&id);
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
            self.apply_user_action(&id, |inst| inst.unarchive())?;
            self.rebuild_flat_items();
            // Re-seat the cursor on the just-unarchived session. After the
            // flat_items rebuild the row jumps from tier 99 to its real
            // tier, so without this the cursor stays at the old index and
            // ends up on whatever row slid into that slot. The session stays
            // Stopped (archive killed its panes); the user restarts it with
            // `e` when they want it back, same as any other stopped session.
            self.select_session_by_id(&id);
            return Ok(());
        }

        // Tear down all tmux before flipping archived. #1868.
        if let Some(inst) = self.instances.get(&id) {
            inst.kill_all_tmux_sessions();
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
            self.rebuild_flat_items();
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
            self.rebuild_flat_items();
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

    /// Move a session to the trash: stop its tmux sessions (a structured-view
    /// worker is reaped by the daemon reconciler once the row reads trashed)
    /// and set `trashed_at`. Durable artifacts are kept so it can be
    /// restored. The Trash section's collapse state is left untouched: like
    /// single-row archive, the section header's count is the feedback, so a
    /// user who collapsed it stays collapsed (#2489).
    pub(super) fn trash_session_by_id(&mut self, id: &str) {
        if let Err(e) = self.apply_user_action(id, |inst| inst.trash()) {
            tracing::warn!(target: "tui.session", session = %id, "trash failed: {e}");
            return;
        }
        if let Some(inst) = self.instances.get(id) {
            inst.kill_all_tmux_sessions();
        }
        // The session is durably trashed and its agent stopped; relocate its
        // worktree out of the active dir. The move + repointed project_path
        // persist through the same diff path. Best-effort: a failure leaves the
        // worktree in place and a later reconcile pass can move it.
        let mut relocate_warning: Option<String> = None;
        let _ = self.apply_user_action(id, |inst| {
            if let crate::session::trash::RelocateOutcome::Failed { reason } =
                crate::session::trash::relocate_worktree_to_trash(inst)
            {
                relocate_warning = Some(reason);
            }
        });
        if let Some(reason) = relocate_warning {
            tracing::warn!(target: "tui.session", session = %id, "trash worktree relocation skipped: {reason}");
        }
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
        let Some(profile) = self
            .instances
            .get(&id)
            .filter(|i| i.is_trashed())
            .map(|i| i.source_profile.clone())
        else {
            return;
        };
        // Restore is NOT routed through `apply_user_action` here: that persists
        // via `merge_user_action_diff`, which deliberately drops `op_claim`, so
        // the symmetric claim would never reach disk. Drive storage directly,
        // mirroring the CLI restore. See #2541.
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
            restore_from_trash_with_storage(storage, &id)
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
                    inst.clear_op_claim_if_owned(ClaimOp::Restore);
                }
                self.rebuild_flat_items();
                self.select_session_by_id(&id);
            }
            RestoreFromTrash::AlreadyGone => {
                self.drop_peer_deleted_rows(std::slice::from_ref(&id));
                self.rebuild_flat_items();
            }
            RestoreFromTrash::PurgeInProgress => {
                self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                    "Restore Failed",
                    "This session is being purged by another process; it was not restored.",
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
    /// the same off-thread deletion path as a single permanent delete: win the
    /// Purge claim under the flock, mark it Deleting, and hand the teardown to
    /// the shared `deletion_poller`, whose completion handler finalizes each
    /// row (the #2534 restore-race recheck and transcript purge included).
    /// Cleanup options are resolved per row from its repo config, mirroring the
    /// CLI `empty-trash`, with force removal so a dirty worktree can't keep a
    /// row pinned.
    pub(super) fn empty_trash_all(&mut self) {
        let trashed: Vec<Instance> = self
            .instances
            .values()
            .filter(|i| i.is_trashed())
            .cloned()
            .collect();
        if trashed.is_empty() {
            return;
        }
        for inst in trashed {
            let id = inst.id.clone();
            // A restart cascade still running on the worker would race the
            // teardown against the container it is mid-creating; skip that row
            // rather than orphan resources, the same guard `delete_selected`
            // applies to a single delete.
            if self.restart_in_flight.contains(&id) {
                continue;
            }
            match self.claim_trashed_purge(&id, true) {
                Ok(crate::session::claim::PurgeClaimDecision::Claimed) => {
                    self.purge_claimed.insert(id.clone());
                }
                Ok(crate::session::claim::PurgeClaimDecision::Restored)
                | Ok(crate::session::claim::PurgeClaimDecision::RestoreInProgress) => continue,
                Ok(crate::session::claim::PurgeClaimDecision::AlreadyGone) => {
                    self.drop_peer_deleted_rows(std::slice::from_ref(&id));
                    continue;
                }
                Err(()) => continue,
            }

            self.set_instance_status(&id, Status::Deleting);

            let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
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
        // Rows now show Deleting until the poller reports each one done and the
        // completion handler drops them. Rebuild once so any AlreadyGone rows
        // dropped above leave the list, then re-anchor the cursor.
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
                .filter(|i| super::project_group_name(i) == group_path)
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
    PurgeInProgress,
    WorktreeFailed {
        reason: String,
    },
    PersistFailed,
}

/// Restore a trashed session under the storage flock: win the Restore claim,
/// move the worktree back off-lock, then commit untrash + release the claim,
/// ownership-guarded. Driven directly against storage (not `apply_user_action`)
/// because the TUI's `merge_user_action_diff` path deliberately drops
/// `op_claim`; the claim/commit decisions are the shared `session::claim`
/// helpers so all three surfaces agree. See #2541.
fn restore_from_trash_with_storage(storage: &Storage, id: &str) -> RestoreFromTrash {
    let claim = match storage.update(|insts, _groups| {
        Ok(crate::session::claim::decide_restore_claim(
            insts,
            id,
            chrono::Utc::now(),
        ))
    }) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "tui.home", id = %id, "restore claim failed: {e}");
            return RestoreFromTrash::PersistFailed;
        }
    };
    match claim {
        crate::session::claim::RestoreClaimDecision::Claimed => {}
        crate::session::claim::RestoreClaimDecision::AlreadyGone => {
            return RestoreFromTrash::AlreadyGone
        }
        crate::session::claim::RestoreClaimDecision::PurgeInProgress => {
            return RestoreFromTrash::PurgeInProgress
        }
    }

    // Load the claimed row for the unlocked worktree move. Distinguish a
    // storage error (transient: release our claim and bail as PersistFailed, so
    // a live trashed row is not dropped from the view) from a genuinely absent
    // row (a peer purged it: AlreadyGone). See #2541.
    let loaded = match storage.load() {
        Ok(all) => all.into_iter().find(|i| i.id == id),
        Err(e) => {
            tracing::warn!(target: "tui.home", id = %id, "restore load failed: {e}");
            let _ = storage.update(|insts, _groups| {
                if let Some(stored) = insts.iter_mut().find(|i| i.id == id) {
                    stored.clear_op_claim_if_owned(ClaimOp::Restore);
                }
                Ok(())
            });
            return RestoreFromTrash::PersistFailed;
        }
    };
    let Some(mut inst) = loaded else {
        return RestoreFromTrash::AlreadyGone;
    };

    if let crate::session::trash::RestoreOutcome::Failed { reason } =
        crate::session::trash::restore_worktree_location(&mut inst)
    {
        let _ = storage.update(|insts, _groups| {
            if let Some(stored) = insts.iter_mut().find(|i| i.id == id) {
                stored.clear_op_claim_if_owned(ClaimOp::Restore);
            }
            Ok(())
        });
        return RestoreFromTrash::WorktreeFailed { reason };
    }
    let restored_path = inst.project_path.clone();
    let restored_pre = inst.pre_trash_project_path.clone();

    match storage.update(|insts, _groups| {
        Ok(crate::session::claim::finalize_restore_commit(
            insts,
            id,
            &restored_path,
            &restored_pre,
        ))
    }) {
        Ok(crate::session::claim::RestoreCommit::Committed) => RestoreFromTrash::Restored {
            project_path: restored_path,
            pre_trash_project_path: restored_pre,
        },
        Ok(crate::session::claim::RestoreCommit::PurgeStoleClaim) => {
            RestoreFromTrash::PurgeInProgress
        }
        Ok(crate::session::claim::RestoreCommit::AlreadyGone) => RestoreFromTrash::AlreadyGone,
        Err(e) => {
            tracing::warn!(target: "tui.home", id = %id, "restore commit failed: {e}");
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

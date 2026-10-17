//! Session operations for HomeView (create, delete, rename)

use crate::session::builder::{self, InstanceParams};
use crate::session::conversation_carry;
use crate::session::{
    duplicate_session_error, is_duplicate_session, list_profiles, GroupMovePlan, GroupTree,
    Instance, Item, LifecycleOperation, StartBlocked, Status, Storage,
};
use crate::tui::deletion_poller::DeletionRequest;
use crate::tui::dialogs::{
    DeleteOptions, GroupDeleteAction, GroupDeleteOptions, InfoDialog, NewSessionData,
};
use crate::tui::restart_poller::RestartRequest;

use super::HomeView;

pub(super) struct RenameResult {
    session_id: String,
    outcome: Result<crate::session::rename::Committed, String>,
}

impl crate::tui::worker::SessionScoped for RenameResult {
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

pub(super) fn perform_rename(request: crate::session::rename::Request) -> RenameResult {
    let session_id = request.baseline.id.clone();
    RenameResult {
        session_id,
        outcome: crate::session::rename::perform(request, |_, _| Ok(()))
            .map_err(|error| format!("{error:#}")),
    }
}

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

enum PersistGroupDelete {
    Ready(Vec<Instance>),
    Creating,
    Restarting,
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

        let mut build_result = builder::build_instance(
            params,
            &existing_titles,
            &existing_branches,
            &target_profile,
        )?;
        if structured {
            builder::structured::apply_structured_choice(&mut build_result.instance);
        }
        let completion = build_result
            .run_on_create(|_| Ok(()))
            .and_then(|()| build_result.publish());
        if let Err(error) = completion {
            return match build_result.rollback() {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("Creation rollback could not finish: {cleanup:#}")))
                }
            };
        }
        let instance = build_result.instance;
        let session_id = instance.id.clone();
        self.storages
            .insert(target_profile.clone(), build_result.profile_storage);
        self.publish_persisted_instance(instance.clone());
        self.rebuild_group_trees();
        if !instance.group_path.is_empty() {
            if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                tree.create_group(&instance.group_path);
            }
        }

        crate::tips::record_session_creations(1);
        self.reload()?;
        // reload()'s selection fallback lands on the nearest index, often the new
        // session's group folder, so pin the selection the caller attaches to. Same as
        // the async branch in apply_creation_results.
        self.select_and_reveal_session(&session_id);
        Ok(session_id)
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
    ) -> anyhow::Result<()> {
        let id = match &self.selected_session {
            Some(id) => id.clone(),
            None => return Ok(()),
        };

        // A cascade for this row is already on the worker. The 1.5s debounce below does
        // not cover a deliberate second press during a multi-second pull, and a duplicate
        // request would restart the row into the container the first one is building.
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
        let restart_edit_baseline = self
            .get_instance(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let current_profile = restart_edit_baseline.source_profile.clone();
        let profile_move_target = new_profile
            .filter(|target| *target != current_profile.as_str())
            .map(str::to_string);
        if let Some(target_profile) = profile_move_target.as_ref() {
            let profiles = list_profiles()?;
            if !profiles.contains(target_profile) {
                anyhow::bail!("Profile '{}' does not exist", target_profile);
            }
        }

        let profile_move_ownership = if profile_move_target.is_some() {
            Some(
                crate::session::storage::try_acquire_ownership_lock()?.ok_or_else(|| {
                    anyhow::anyhow!("Session operation in progress; no changes were saved")
                })?,
            )
        } else {
            None
        };
        let profile_move_identity = profile_move_ownership
            .as_ref()
            .map(|ownership| {
                crate::session::storage::try_acquire_session_identity_lock_with_ownership(
                    ownership,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!("Session operation in progress; no changes were saved")
                })
            })
            .transpose()?;
        let profile_move_guards = profile_move_ownership
            .as_ref()
            .map(|ownership| {
                self.try_lock_session_mutation_and_reload_with_ownership(ownership, &id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Session operation in progress; no changes were saved")
                    })
            })
            .transpose()?;
        let restart_edit_authoritative = self
            .get_instance(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        if let (Some(target_profile), Some(ownership)) = (
            profile_move_target.as_deref(),
            profile_move_ownership.as_ref(),
        ) {
            let target_rows =
                Storage::open_with_ownership(target_profile, self.file_watch.clone(), ownership)?
                    .load_strict_for_worktree_ownership()?;
            if is_duplicate_session(
                target_rows.iter(),
                &restart_edit_authoritative.title,
                &restart_edit_authoritative.project_path,
                None,
            ) {
                return Err(duplicate_session_error(&restart_edit_authoritative.title));
            }
        }

        let tool_swapped = new_tool.is_some_and(|tool| tool != restart_edit_authoritative.tool);
        // Decided against the pre-swap row: once the swap has run, the outgoing
        // account's config root is no longer reachable from the instance.
        let swap_kind = match new_tool.filter(|_| tool_swapped) {
            Some(tool) => conversation_carry::classify(
                &restart_edit_authoritative,
                profile_move_target.as_deref().unwrap_or(&current_profile),
                tool,
            ),
            None => conversation_carry::ToolSwap::Park,
        };
        let (account_swap, mut carry_plan) = match swap_kind {
            conversation_carry::ToolSwap::Park => (false, None),
            conversation_carry::ToolSwap::KeepConversation(carry) => (true, carry),
        };

        // A cross-profile restart stages on a detached candidate: do not persist a tool
        // swap into the source row before the target transaction accepts it.
        if let (Some(target_profile), Some(ownership)) = (
            profile_move_target.as_deref(),
            profile_move_ownership.as_ref(),
        ) {
            if !self.storages.contains_key(target_profile) {
                self.storages.insert(
                    target_profile.to_string(),
                    Storage::open_with_ownership(
                        target_profile,
                        self.file_watch.clone(),
                        ownership,
                    )?,
                );
            }

            let mut requested = restart_edit_authoritative.clone();
            if wake_snooze {
                requested.unsnooze();
            }
            if let Some(target_tool) = new_tool {
                if target_tool != restart_edit_authoritative.tool.as_str() {
                    if account_swap {
                        requested.swap_account(target_tool);
                    } else {
                        requested.swap_tool(target_tool);
                    }
                }
            }
            if let Some(command) = new_command_override {
                requested.command = command.to_string();
            }
            if let Some(extra) = new_extra_args {
                requested.extra_args = extra.to_string();
            }
            requested.touch_last_accessed();
            self.move_to_profile_with_ownership(
                ownership,
                &id,
                target_profile,
                requested,
                Some(&restart_edit_authoritative),
                account_swap,
            )?;
            // The committed row is the authority the launch resumes from, and
            // the capture pollers can have refreshed its conversation since the
            // snapshot the plan froze; see `ConversationCarry::retarget`.
            if let Some(carry) = carry_plan.as_mut() {
                if let Some(moved) = self.get_instance(&id) {
                    carry.retarget(conversation_carry::conversation_ids(moved));
                }
            }
        } else {
            // Outside Attention sort, restart on a snoozed row clears the
            // snooze flag so persisted state matches the visible restart.
            if wake_snooze {
                self.mutate_instance(&id, |inst| inst.unsnooze());
            }

            if let Some(target_tool) = new_tool {
                let current_tool = self
                    .get_instance(&id)
                    .map(|i| i.tool.clone())
                    .unwrap_or_default();
                if target_tool != current_tool {
                    if account_swap {
                        self.mutate_instance(&id, |inst| inst.swap_account(target_tool));
                    } else {
                        self.mutate_instance(&id, |inst| inst.swap_tool(target_tool));
                    }
                    let disk_ids = self.persist_tool_swap(&id, target_tool, account_swap);
                    if let Some(carry) = carry_plan.as_mut() {
                        carry.retarget(disk_ids);
                    }
                }
            }
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
        }
        self.restart_cooldown_at.insert(id.clone(), now);
        if profile_move_target.is_none() {
            self.mutate_instance(&id, |inst| inst.touch_last_accessed());
            self.save()?;
        }
        drop(profile_move_guards);
        drop(profile_move_identity);
        drop(profile_move_ownership);
        if profile_move_target.is_some() {
            self.reload_preserving_profile_move_runtime(std::slice::from_ref(&id))?;
        }

        // The cascade shells out to docker and runs the before_start hook, so it stays
        // off the event loop: show Starting locally, let the worker reserve and persist
        // it, and reconcile through `apply_restart_results`.
        let size = crate::terminal::get_size();

        // Starting plus a fresh last_start_time keeps the StatusPoller from flipping the
        // row to Error before the worker finishes.
        self.mutate_instance(&id, |inst| {
            inst.status = Status::Starting;
            inst.last_error = None;
            inst.last_start_time = Some(std::time::Instant::now());
        });

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
            skip_on_launch: false,
            bound_hooks: true,
            discard_sandbox_container: tool_swapped,
            conversation_carry: carry_plan,
        });
        Ok(())
    }

    /// Land an engine swap's session bookkeeping on the disk row.
    ///
    /// `account_swap` picks which swap the disk row takes: the parking one, or the one
    /// that keeps the conversation because only the account changed. Returns the
    /// conversation ids the disk row held before the swap, which a carry uses in place
    /// of the ones its plan froze; see
    /// [`crate::session::conversation_carry::ConversationCarry::retarget`].
    ///
    /// `save()` leaves `agent_session_id` and friends to their CAS writers, so without
    /// this write `reconcile_from_disk` restores the old engine's sid and the new engine
    /// resumes a foreign one. It runs against the disk row because the capture pollers
    /// may hold a fresher sid than this snapshot; parking whatever disk holds is what
    /// makes a swap-back restore the real conversation. Best-effort: a failed write
    /// leaves the stale sid and the restart's resume probe recovers by starting fresh.
    fn persist_tool_swap(&self, id: &str, new_tool: &str, account_swap: bool) -> Vec<String> {
        let Some(profile) = self.instances.get(id).map(|i| i.source_profile.clone()) else {
            return Vec::new();
        };
        let Some(storage) = self.storages.get(&profile) else {
            tracing::warn!(
                target: "tui.home",
                profile = %profile,
                id = %id,
                "persist_tool_swap: no storage registered for profile; \
                 the old engine's session id stays on disk"
            );
            return Vec::new();
        };
        let id_owned = id.to_string();
        let new_tool = new_tool.to_string();
        let row_profile = profile.clone();
        let observed = storage.update(|instances, _groups| {
            let mut observed = Vec::new();
            if let Some(disk) = instances.iter_mut().find(|i| i.id == id_owned) {
                observed = crate::session::conversation_carry::conversation_ids(disk);
                // `source_profile` is `skip_serializing`, so a storage-loaded row
                // resolves `agent_detect_as` against the default profile and would pin
                // the wrong built-in; `detect_as` is not in `reconcile_from_disk`'s carry
                // set, so the next launch reads that value. Restore it before the swap.
                disk.source_profile = row_profile.clone();
                if account_swap {
                    disk.swap_account(&new_tool);
                } else {
                    disk.swap_tool(&new_tool);
                }
            }
            Ok(observed)
        });
        match observed {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    target: "tui.home",
                    id = %id,
                    "persist_tool_swap: failed to move the old engine's session state aside: {e}"
                );
                Vec::new()
            }
        }
    }

    pub(super) fn delete_selected(&mut self, options: &DeleteOptions) -> anyhow::Result<()> {
        if let Some(id) = &self.selected_session {
            let id = id.clone();

            // Deleting a row mid-restart would fire docker commands against the
            // container the restart worker is creating and orphan resources.
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
                self.request_deletion(request);
            }
        }
        Ok(())
    }

    pub(super) fn delete_selected_group(
        &mut self,
        options: &GroupDeleteOptions,
    ) -> anyhow::Result<()> {
        if options.action == GroupDeleteAction::DeleteSessions {
            return self.delete_group_with_sessions(options);
        }
        if let Some(group_path) = self.selected_group.take() {
            let target_group = if options.action == GroupDeleteAction::MoveToParent {
                group_path.rsplit_once('/').map_or("", |(parent, _)| parent)
            } else {
                ""
            };
            let owning_profile = self.selected_group_profile.take();
            let prefix = format!("{}/", group_path);
            let is_member = group_membership(&group_path, &prefix, owning_profile.as_deref());
            let ids_to_move: Vec<String> = self
                .instances
                .values()
                .filter(|i| is_member(i))
                .map(|i| i.id.clone())
                .collect();
            self.bulk_apply_user_action(&ids_to_move, |inst| {
                inst.group_path = target_group.to_string();
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

    /// Commit one profile's group deletion before any purge is queued, so a watcher
    /// reload or a restart cannot rebuild the group from an unchanged `groups.json`.
    /// Blockers are re-checked against durable rows because a Creating member may exist
    /// only on disk. `Status::Deleting` stays an in-memory overlay owned by
    /// `PurgeTransaction`.
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
                self.request_deletion(DeletionRequest {
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

    pub(super) fn request_deletion(&mut self, request: DeletionRequest) {
        self.deletes_in_flight.insert(
            request.session_id.clone(),
            super::DeleteAttempt {
                forced: request.force_delete,
                trashed_at: request.instance.trashed_at,
                trash_lifecycle_id: request.instance.trash_lifecycle_id.clone(),
            },
        );
        self.deletion_poller.request_deletion(request);
    }

    /// Whether a trashed row's last delete was forced, when it failed in the row's current
    /// trash lifecycle and no other delete for it is in flight.
    pub(super) fn failed_delete_forced(&self, inst: &Instance) -> Option<bool> {
        if self.deletes_in_flight.contains_key(&inst.id) {
            return None;
        }
        self.failed_deletes
            .get(&inst.id)
            .filter(|attempt| {
                inst.trashed_at.is_some()
                    && attempt.trashed_at == inst.trashed_at
                    && inst.trash_lifecycle_id.is_some()
                    && attempt.trash_lifecycle_id == inst.trash_lifecycle_id
            })
            .map(|attempt| attempt.forced)
    }

    /// Force-remove a session from storage, for rows stuck in Deleting. Worktree and
    /// branch cleanup are skipped because the original deletion already attempted them;
    /// tmux and sandbox teardown run off-thread so a hung call cannot block input.
    pub(super) fn force_remove_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        let instance = self.instances.get(session_id).cloned();
        self.failed_deletes.remove(session_id);
        self.remove_instance(session_id);
        self.rebuild_group_trees();
        self.save()?;
        self.reload()?;

        if let Some(inst) = instance {
            spawn_force_teardown(inst);
        }
        Ok(())
    }

    /// Remove a trashed row whose forced delete failed, skipping worktree and branch
    /// cleanup. The guarded transaction runs on the drop worker, since a peer purge can
    /// hold the lifecycle lock for its whole teardown; `apply_drop_results` lands it.
    fn drop_failed_trashed_session(&mut self, inst: &Instance) {
        self.deletes_in_flight.insert(
            inst.id.clone(),
            super::DeleteAttempt {
                forced: true,
                trashed_at: inst.trashed_at,
                trash_lifecycle_id: inst.trash_lifecycle_id.clone(),
            },
        );
        self.set_instance_status(&inst.id, Status::Deleting);
        self.drop_poller.request(
            inst.id.clone(),
            DropRequest {
                instance: inst.clone(),
            },
        );
    }

    pub fn apply_drop_results(&mut self) -> bool {
        let Ok(result) = self.drop_poller.try_recv() else {
            return false;
        };
        let inst = result.instance;
        self.deletes_in_flight.remove(&inst.id);
        if should_clear_drop_retry_state(&result.dropped, &inst, self.instances.get(&inst.id)) {
            self.failed_deletes.remove(&inst.id);
        }
        match result.dropped {
            Ok(true) => {
                self.instances.shift_remove(&inst.id);
                self.rebuild_group_trees();
                spawn_force_teardown(inst);
            }
            Ok(false) => {
                self.set_instance_status(&inst.id, inst.status);
                self.info_dialog = Some(InfoDialog::new(
                    "Session not removed",
                    &format!(
                        "'{}' changed in another process (restored, re-trashed or busy), so it was kept.",
                        inst.title
                    ),
                ));
            }
            Err(e) => {
                self.set_instance_status(&inst.id, inst.status);
                tracing::error!(target: "tui.home", session = %inst.id, "empty trash drop failed: {e}");
            }
        }
        self.rebuild_flat_items();
        true
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

            let ownership =
                crate::session::storage::try_acquire_ownership_lock()?.ok_or_else(|| {
                    anyhow::anyhow!("Session operation in progress; no changes were saved")
                })?;
            let identity_guard =
                crate::session::storage::try_acquire_session_identity_lock_with_ownership(
                    &ownership,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!("Session operation in progress; no changes were saved")
                })?;
            affected_ids.sort();
            let mut mutation_guards = Vec::with_capacity(affected_ids.len());
            for id in &affected_ids {
                mutation_guards.push(
                    self.try_lock_session_mutation_and_reload_with_ownership(&ownership, id)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Session operation in progress; no changes were saved")
                        })?,
                );
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
                    Storage::open_with_ownership(
                        target_profile,
                        self.file_watch.clone(),
                        &ownership,
                    )?,
                );
            }
            // Run the transaction for its durable effect only: the reload below
            // republishes the moved rows and re-merges runtime-only state onto them.
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
                source.move_instances_to_with_ownership(
                    &ownership,
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
            drop(mutation_guards);
            drop(identity_guard);
            drop(ownership);
            self.reload_preserving_profile_move_runtime(&affected_ids)?;
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

        // Capture old_path and its descendants before the rebuild, which derives groups
        // from `instance.group_path`, already migrated by the loop above.
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

    /// Edit the selected session's worktree workdir name: move the worktree directory
    /// and, optionally, rename its git branch, persisting both. See #1723.
    pub(super) fn set_worktree_name_for_selected(
        &mut self,
        new_name: &str,
        rename_branch: bool,
    ) -> anyhow::Result<()> {
        self.queue_rename(crate::session::rename::Edit::Workdir {
            name: new_name.to_string(),
            rename_branch,
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
    ) -> anyhow::Result<()> {
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
        if self.attach_project_in_flight.contains(id) {
            anyhow::bail!("An attach is already running for this session; wait for it to finish");
        }

        let storage = self
            .storages
            .get(&instance.source_profile)
            .ok_or_else(|| anyhow::anyhow!("session profile storage is unavailable"))?
            .clone();
        // Everything blocking runs on the poller thread: Git worktree creation, worker stop,
        // conversion and restart. Results reload the durable profile and report the outcome.
        self.attach_project_in_flight.insert(id.to_string());
        self.attach_project_poller.request_attach(
            crate::session::attach_project::AttachProjectRequest {
                session_id: id.to_string(),
                storage,
                instance: instance.clone(),
                repo_path: repo_path.to_path_buf(),
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
        let title = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .filter(|_| !new_title.is_empty())
            .map(|_| new_title.to_string());
        self.queue_rename(crate::session::rename::Edit::Session {
            title,
            group: new_group.map(str::to_string),
            profile: new_profile.map(str::to_string),
            rename_branch,
            branch: None,
        })
    }

    fn queue_rename(&mut self, edit: crate::session::rename::Edit) -> anyhow::Result<()> {
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let baseline = self
            .get_instance(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        anyhow::ensure!(
            !self.rename_worker.is_pending(&id),
            "A rename is already in progress"
        );
        anyhow::ensure!(
            !baseline.has_fresh_lifecycle_reservation(chrono::Utc::now()),
            "Session has a lifecycle operation in progress; retry when it finishes"
        );
        self.rename_worker.request(
            id,
            crate::session::rename::Request {
                baseline,
                edit,
                file_watch: self.file_watch.clone(),
            },
        );
        Ok(())
    }

    pub(in crate::tui) fn apply_rename_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut changed = false;
        loop {
            match self.rename_worker.try_recv() {
                Ok(result) => {
                    changed = true;
                    match result.outcome {
                        Ok(committed) => {
                            let Some(current) = self.instances.get(&result.session_id) else {
                                continue;
                            };
                            if current.created_at != committed.instance.created_at
                                || current.lifecycle_generation
                                    > committed.instance.lifecycle_generation
                                || current.source_profile != committed.original.source_profile
                            {
                                continue;
                            }
                            let mut row = committed.instance;
                            row.merge_runtime_from_reload(current);
                            let baseline =
                                if current.lifecycle_generation == row.lifecycle_generation {
                                    &committed.original
                                } else {
                                    &committed.baseline
                                };
                            row.merge_user_action_diff(baseline, current);
                            if row.source_profile != committed.original.source_profile {
                                if let Some(tree) =
                                    self.group_trees.get_mut(&committed.original.source_profile)
                                {
                                    tree.prune_empty_group(
                                        &committed.original.group_path,
                                        self.instances
                                            .values()
                                            .filter(|instance| {
                                                instance.id != result.session_id
                                                    && instance.source_profile
                                                        == committed.original.source_profile
                                            })
                                            .map(|instance| instance.group_path.as_str()),
                                    );
                                }
                            }
                            if self
                                .active_profile
                                .as_deref()
                                .is_some_and(|profile| profile != row.source_profile)
                            {
                                self.instances.shift_remove(&result.session_id);
                                self.rebuild_group_trees();
                                self.rebuild_flat_items_keeping_cursor();
                                if let Some(warning) = committed.warning {
                                    self.info_dialog = Some(InfoDialog::new(
                                        "Rename Saved with Warning",
                                        &warning,
                                    ));
                                }
                                continue;
                            }
                            let profile = row.source_profile.clone();
                            self.storages.insert(profile.clone(), committed.storage);
                            self.group_trees
                                .entry(profile)
                                .or_insert_with(|| GroupTree::new_with_groups(&[], &[]));
                            self.instances.insert(result.session_id, row);
                            self.rebuild_group_trees();
                            self.rebuild_flat_items_keeping_cursor();
                            if let Some(warning) = committed.warning {
                                self.info_dialog =
                                    Some(InfoDialog::new("Rename Saved with Warning", &warning));
                            }
                        }
                        Err(error) => {
                            self.info_dialog = Some(InfoDialog::new("Rename Failed", &error))
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.rename_worker.take_pending().is_empty() {
                        self.info_dialog = Some(InfoDialog::new(
                            "Rename Failed",
                            "The rename worker stopped; reload and retry.",
                        ));
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub(super) fn finish_rename_for_test(&mut self) {
        let id = self
            .selected_session
            .clone()
            .expect("selected rename session");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.rename_worker.is_pending(&id) {
            self.apply_rename_results();
            assert!(
                std::time::Instant::now() < deadline,
                "rename worker did not finish"
            );
            std::thread::yield_now();
        }
    }

    /// Snooze keybind: an already-snoozed row wakes immediately, otherwise the duration
    /// picker opens and `snooze_session_for` runs on submit.
    ///
    /// A snooze is a temporary archive: `snoozed_until = now + minutes` sinks the row to
    /// tier 99, renders it italic+dim with a `z ` prefix and the remaining time, and it
    /// wakes lazily when the timer elapses. The duration is resolved at snooze time, so
    /// changing the config default does not extend one in flight.
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

    /// Apply a snooze with an explicit duration, on the picker's submit. The only place
    /// the TUI mutates `snoozed_until`. Jumps to the next needs-attention row once this
    /// one sinks, so triage can continue.
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

    /// Toggle the favorite flag on the cursor's session. Favorite survives an unsnooze
    /// but not an archive; that mutual exclusion lives in `Instance::archive()`.
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

    /// The session the cursor should land on once the cursor's row is archived: the
    /// nearest visible non-archived session below, else above. Rows inside collapsed
    /// groups and rows already under the Archived section are not candidates, so the
    /// cursor never jumps into either. `None` leaves the caller to clamp the index.
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

    /// Manual unread toggle (`U`), symmetric in both directions. The row's
    /// `theme.unread` color is the feedback, so there is no toast. No-op when disabled.
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
        // Hold the row for the current visit so the dwell cannot undo a fresh `u`; the
        // hold is released once the cursor leaves (see `tick_unread_dwell`).
        if self.get_instance(&id).is_some_and(|i| i.is_unread()) {
            self.manual_unread_hold = Some(id.clone());
        } else if self.manual_unread_hold.as_deref() == Some(id.as_str()) {
            self.manual_unread_hold = None;
        }
        self.rebuild_flat_items();
        // Under Attention sort the toggle changes the row's rank, so reseat the cursor
        // by id to keep the next action on this session.
        self.select_session_by_id(&id);
        Ok(())
    }

    /// Toggle archive on the cursor's session. Archive tears down every tmux session
    /// (agent plus ancillary) but keeps worktree, branch and container. Unarchive does
    /// not respawn: press `e`, or send a message to auto-unarchive. See #1868.
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
            self.apply_user_action(&id, |inst| inst.unarchive())?;
            self.rebuild_flat_items();
            // Re-seat the cursor on the unarchived row: the rebuild moves it from tier
            // 99 to its real tier. It stays Stopped until the user restarts it with `e`.
            self.select_session_by_id(&id);
            return Ok(());
        }

        // Tear down all tmux before flipping archived (#1868), holding the lifecycle lock
        // through the archive so `aoe send` cannot relaunch or type into the session between.
        let lifecycle_lock = match self.instances.get(&id) {
            Some(inst) => {
                let storage = Storage::new(&inst.effective_profile(), self.file_watch.clone())?;
                let lock = storage.acquire_instance_lifecycle_lock(&id)?;
                inst.stop_all_tmux_sessions_locked(&storage);
                Some(lock)
            }
            None => None,
        };

        // Decide where the cursor lands before the row sinks, against the pre-archive
        // list. Only the non-Attention branch uses it; Attention re-picks from the top.
        let successor = (self.sort_order != crate::session::config::SortOrder::Attention)
            .then(|| self.archive_successor_session(&id))
            .flatten();

        self.apply_user_action(&id, |inst| inst.archive())?;
        drop(lifecycle_lock);
        if self.sort_order == crate::session::config::SortOrder::Attention {
            // Attention sort is a triage flow: the cursor advances to the next item
            // that needs attention, which is always a live row.
            self.rebuild_flat_items();
            self.select_top_attention(None);
            // select_top_attention is a no-op when no session row is visible, which
            // would strand the selection on the now-invisible archived row and leave the
            // cursor past the list; clamp and re-resolve like the fallback below.
            if self.selected_session.as_deref() == Some(id.as_str()) {
                self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
                self.update_selected();
            }
        } else {
            // Advance to the next session rather than following the archived row into
            // the section: archiving reads as "done with this one". The preview retargets
            // itself each frame and the worker drops stale frames, so there is no
            // dead-pane flash (#2025). The section is not revealed; its count is the
            // feedback.
            self.rebuild_flat_items();
            match successor {
                Some(next) => self.select_session_by_id(&next),
                None => {
                    // No other active session: clamp and let `update_selected` resolve
                    // whatever sits at the cursor now (usually the Archived header).
                    self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
                    self.update_selected();
                }
            }
        }
        Ok(())
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
    /// container's grace period, which froze the input thread inline (#1496). A
    /// structured-view worker is reaped by the daemon reconciler once the row reads
    /// trashed.
    pub(super) fn trash_session_by_id(&mut self, id: &str) {
        let Some((profile, request_instance)) = self
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
        let request = match crate::session::trash::try_prepare_trash(storage, &request_instance) {
            Ok(Some(request)) => request,
            Ok(None) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Session operation in progress",
                    "Another session operation is in progress. No changes were saved. Try again when it finishes.",
                ));
                return;
            }
            Err(error) => {
                self.info_dialog = Some(InfoDialog::new("Trash Failed", &format!("{error:#}")));
                return;
            }
        };
        if let Some(instance) = self.instances.get_mut(id) {
            instance.trashed_at = request.instance.trashed_at;
            instance.trash_lifecycle_id = request.instance.trash_lifecycle_id.clone();
            instance.lifecycle_generation = request.instance.lifecycle_generation;
            instance.lifecycle_reservation = request.instance.lifecycle_reservation.clone();
        }
        self.trash_poller.request_trash(request);
        self.rebuild_flat_items();
        self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
        self.update_selected();
    }

    /// Restore the selected trashed session, clearing `trashed_at` so it returns to its
    /// prior bucket. No-op when the selection is not trashed; the session stays stopped
    /// until the user restarts it with `e`. See #2489.
    pub(super) fn restore_selected_from_trash(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        self.restore_from_trash_by_id(&id);
    }

    fn restore_from_trash_by_id(&mut self, id: &str) {
        let Some(baseline) = self
            .instances
            .get(id)
            .filter(|row| row.is_trashed())
            .cloned()
        else {
            return;
        };
        if self.restore_worker.is_pending(id) {
            return;
        }
        let Some(storage) = self.storages.get(&baseline.source_profile).cloned() else {
            return;
        };
        self.restore_worker
            .request(id.to_string(), RestoreRequest { storage, baseline });
    }

    pub(in crate::tui) fn apply_restore_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut changed = false;
        loop {
            match self.restore_worker.try_recv() {
                Ok(result) => {
                    changed = true;
                    let Some(current) = self.instances.get_mut(&result.baseline.id) else {
                        continue;
                    };
                    if current.created_at != result.baseline.created_at
                        || current.source_profile != result.baseline.source_profile
                        || result.storage.verify_profile_available().is_err()
                    {
                        continue;
                    }
                    match result.outcome {
                        Ok(restored) => {
                            if current.lifecycle_generation > restored.lifecycle_generation
                                || current.trashed_at != result.baseline.trashed_at
                                || current.trash_lifecycle_id != result.baseline.trash_lifecycle_id
                            {
                                continue;
                            }
                            current.project_path = restored.project_path;
                            current.pre_trash_project_path = restored.pre_trash_project_path;
                            current.lifecycle_generation = restored.lifecycle_generation;
                            current.lifecycle_reservation = restored.lifecycle_reservation;
                            current.untrash();
                            let was_selected =
                                self.selected_session.as_deref() == Some(&result.baseline.id);
                            self.rebuild_flat_items();
                            if was_selected {
                                self.select_session_by_id(&result.baseline.id);
                            }
                        }
                        Err(crate::session::trash::RestoreError::NotFound) => {
                            self.drop_peer_deleted_rows(std::slice::from_ref(&result.baseline.id));
                            self.rebuild_flat_items();
                        }
                        Err(error) => {
                            let reason = match error {
                                crate::session::trash::RestoreError::Busy(reason) => {
                                    format!("Session is {reason}, so it was not restored.")
                                }
                                crate::session::trash::RestoreError::Worktree(reason) => {
                                    format!("Could not restore the worktree: {reason}")
                                }
                                crate::session::trash::RestoreError::Persist(error) => {
                                    format!("Could not persist the restore: {error}")
                                }
                                crate::session::trash::RestoreError::NotFound => unreachable!(),
                            };
                            self.info_dialog = Some(InfoDialog::new("Restore Failed", &reason));
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.restore_worker.take_pending().is_empty() {
                        self.info_dialog = Some(InfoDialog::new(
                            "Restore Failed",
                            "The restore worker stopped; reload and retry.",
                        ));
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub(super) fn finish_restore_for_test(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let ids: Vec<_> = self.instances.keys().cloned().collect();
        while ids.iter().any(|id| self.restore_worker.is_pending(id)) {
            self.apply_restore_results();
            assert!(
                std::time::Instant::now() < deadline,
                "restore result never drained"
            );
            std::thread::yield_now();
        }
    }

    /// Enqueue each trashed row by ID; failures use the same dialog as selected restore.
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
            self.restore_from_trash_by_id(&id);
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
            // A restart cascade still on the worker would race the teardown against the
            // container it is creating; skip the row, as `delete_selected` does. A second
            // delete would also overwrite the force level tracked for the first.
            if self.restart_in_flight.contains(&id) || self.deletes_in_flight.contains_key(&id) {
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

    /// Archive every active session under the selected group: persist runs inline, then tmux
    /// teardown runs off-thread. Confirmation upstream. See #1868.
    pub(super) fn archive_selected_group(&mut self) -> anyhow::Result<()> {
        let ids = self.active_sessions_in_selected_group();
        if ids.is_empty() {
            return Ok(());
        }
        let kill_targets: Vec<_> = ids
            .iter()
            .filter_map(|id| self.instances.get(id).cloned())
            .map(|instance| {
                let storage = Storage::new(&instance.effective_profile(), self.file_watch.clone())?;
                Ok((instance, storage))
            })
            .collect::<anyhow::Result<_>>()?;
        // Persist under every member's lifecycle lock so `aoe send` cannot relaunch or type
        // into one mid-archive, and tear down only after: a send that won the lock finishes
        // first and its pane is then killed. Locks go in sorted id order, like every other
        // multi-lock holder (startup reservation cleanup), so two holders cannot close a cycle.
        let mut lock_order: Vec<_> = kill_targets.iter().collect();
        lock_order.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        let mut lifecycle_locks = Vec::with_capacity(lock_order.len());
        for (inst, storage) in lock_order {
            lifecycle_locks.push(storage.acquire_instance_lifecycle_lock(&inst.id)?);
        }
        self.bulk_apply_user_action(&ids, |inst| inst.archive())?;
        drop(lifecycle_locks);
        // Off-thread tmux teardown so N x 4 shellouts don't block the input
        // thread. Mirrors `force_remove_session`.
        std::thread::spawn(move || {
            for (inst, storage) in kill_targets {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inst.kill_all_tmux_sessions_with(&storage)
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
        self.reveal_archived_section();
        self.rebuild_flat_items();
        // The project header vanishes once its last active member is archived, so the
        // cursor's old index may point past the list end; clamp and re-resolve.
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
        Ok(())
    }
}

pub(in crate::tui) struct RestoreRequest {
    storage: Storage,
    baseline: Instance,
}

pub(in crate::tui) struct RestoreResult {
    storage: Storage,
    baseline: Instance,
    outcome: Result<Instance, crate::session::trash::RestoreError>,
}

impl crate::tui::worker::SessionScoped for RestoreResult {
    fn session_id(&self) -> &str {
        &self.baseline.id
    }
}

pub(super) fn perform_restore(request: RestoreRequest) -> RestoreResult {
    let owned_trash_generation = request
        .baseline
        .lifecycle_reservation
        .as_ref()
        .filter(|reservation| reservation.op == LifecycleOperation::Trash)
        .map(|reservation| reservation.generation);
    let outcome = crate::session::trash::restore_transition(
        &request.storage,
        &request.baseline,
        owned_trash_generation,
    );
    RestoreResult {
        storage: request.storage,
        baseline: request.baseline,
        outcome,
    }
}

pub(in crate::tui) struct DropRequest {
    instance: Instance,
}

pub(in crate::tui) struct DropResult {
    instance: Instance,
    dropped: anyhow::Result<bool>,
}

impl crate::tui::worker::SessionScoped for DropResult {
    fn session_id(&self) -> &str {
        &self.instance.id
    }
}

/// Remove the durable row under its lifecycle lock and purge claim, and only while it is
/// still in the trash lifecycle the failure was recorded in, so a peer restore between
/// confirm and submit keeps the row and its processes.
pub(super) fn perform_drop(request: DropRequest) -> DropResult {
    let inst = request.instance;
    let id = inst.id.as_str();
    let dropped = (|| -> anyhow::Result<bool> {
        let storage = Storage::open_unwatched(&inst.source_profile)?;
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(id)?;
        let mut dropped = false;
        storage.update(|instances, _groups| {
            let same_lifecycle = instances
                .iter()
                .any(|stored| stored.id == id && stored.same_trash_lifecycle(&inst));
            if same_lifecycle
                && matches!(
                    crate::session::claim::decide_purge_claim(
                        instances,
                        id,
                        true,
                        chrono::Utc::now()
                    )?,
                    crate::session::claim::PurgeClaimDecision::Claimed(_)
                )
            {
                instances.retain(|stored| stored.id != id);
                dropped = true;
            }
            Ok(())
        })?;
        Ok(dropped)
    })();
    DropResult {
        instance: inst,
        dropped,
    }
}

/// Retain failed removal history only while its durable trash lifecycle remains current.
fn should_clear_drop_retry_state(
    dropped: &anyhow::Result<bool>,
    requested: &Instance,
    current: Option<&Instance>,
) -> bool {
    matches!(dropped, Ok(true))
        || !current.is_some_and(|current| requested.same_trash_lifecycle(current))
}

/// Tear down a removed session's tmux and sandbox off-thread so a hung call cannot block
/// input.
fn spawn_force_teardown(inst: Instance) {
    std::thread::spawn(move || {
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inst.kill_all_tmux_sessions_without_lifecycle_row()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::restart_poller::RestartPoller;

    #[test]
    #[serial_test::serial]
    fn input_mutations_return_before_ownership_and_nested_locks_are_released() {
        for action in ["restart-move", "group-move", "trash"] {
            for layer in [
                "root-exclusive",
                "root-shared",
                "identity",
                "title",
                "lifecycle",
                "transition",
                "storage",
                "target-storage",
            ] {
                let app = crate::session::test_support::isolate_app_dir();
                let source = Storage::new_unwatched("owner").unwrap();
                let target = Storage::new_unwatched("target").unwrap();
                let mut instance = Instance::new("input admission", "/unmanaged-input");
                instance.source_profile = "owner".into();
                instance.status = Status::Stopped;
                instance.group_path = "work".into();
                source
                    .update(|rows, groups| {
                        rows.push(instance.clone());
                        *groups = GroupTree::new_with_groups(rows, &[]).get_all_groups();
                        Ok(())
                    })
                    .unwrap();
                let mut view = HomeView::new_for_test(
                    None,
                    crate::tmux::AvailableTools::with_tools(&["claude"]),
                    crate::file_watch::FileWatchService::noop(),
                )
                .unwrap();
                view.group_by = crate::session::config::GroupByMode::Manual;
                view.selected_session = Some(instance.id.clone());
                view.restart_poller =
                    RestartPoller::with_result_for_test(crate::session::restart::RestartResult {
                        session_id: instance.id.clone(),
                        before: Box::new(instance.clone()),
                        instance: Box::new(instance.clone()),
                        outcome: Err("fixture result".into()),
                    });
                view.trash_poller =
                    crate::tui::trash_poller::TrashPoller::with_handler_for_test(|request| {
                        request.cancel().unwrap();
                        crate::session::trash::TrashResult {
                            session_id: request.session_id.clone(),
                            source_profile: request.instance.source_profile.clone(),
                            created_at: request.instance.created_at,
                            generation: request.generation,
                            trash_lifecycle_id: request.instance.trash_lifecycle_id.clone(),
                            relocation: None,
                            relocate_warning: None,
                        }
                    });
                let namespace = crate::session::get_app_dir().unwrap();
                let source_dir = source.sessions_path().parent().unwrap().to_path_buf();
                let target_dir = target.sessions_path().parent().unwrap().to_path_buf();
                let id = instance.id.clone();
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let (released_tx, released_rx) = std::sync::mpsc::channel();
                let holder = std::thread::spawn(move || {
                    let held: Box<dyn std::any::Any> = match layer {
                        "root-exclusive" => {
                            Box::new(crate::session::storage::acquire_ownership_lock().unwrap())
                        }
                        "root-shared" => {
                            Box::new(crate::session::storage::acquire_ownership_read().unwrap())
                        }
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
                                &source_dir,
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
                            crate::session::storage::acquire_storage_flock(
                                &source_dir,
                                ".storage.lock",
                            )
                            .unwrap(),
                        ),
                        "target-storage" => Box::new(
                            crate::session::storage::acquire_storage_flock(
                                &target_dir,
                                ".storage.lock",
                            )
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
                let before = std::fs::read(source.sessions_path()).unwrap();
                let invoke = |view: &mut HomeView| match action {
                    "restart-move" => {
                        view.restart_selected_session(Some("target"), None, None, None)
                    }
                    "group-move" => {
                        view.group_rename_context = Some(super::super::GroupRenameContext {
                            old_path: "work".into(),
                            old_profile: "owner".into(),
                        });
                        view.rename_selected_group(None, Some("target"))
                    }
                    "trash" => {
                        view.trash_session_by_id(&instance.id);
                        Ok(())
                    }
                    _ => unreachable!(),
                };
                let result = invoke(&mut view);
                let no_enqueue = !view.restart_in_flight.contains(&instance.id)
                    && !view.trash_poller.is_pending(&instance.id);
                let _ = release_tx.send(());
                let returned_while_held = released_rx.recv().unwrap();
                holder.join().unwrap();
                // Trash does not need title or target-store ownership.
                let contended = !(action == "trash" && matches!(layer, "title" | "target-storage"));
                if contended {
                    assert!(
                        returned_while_held,
                        "{action}/{layer}: input waited for a lock"
                    );
                    assert!(no_enqueue, "{action}/{layer}: contended action was queued");
                    assert_eq!(
                        std::fs::read(source.sessions_path()).unwrap(),
                        before,
                        "{action}/{layer}"
                    );
                    if action == "trash" {
                        assert!(view.info_dialog.is_some());
                    } else {
                        assert!(result.is_err());
                    }
                    view.info_dialog = None;
                    invoke(&mut view).unwrap();
                } else {
                    result.unwrap();
                }
                match action {
                    "trash" => assert!(source.load().unwrap()[0].is_trashed()),
                    _ => assert!(target
                        .load()
                        .unwrap()
                        .iter()
                        .any(|row| row.id == instance.id)),
                }
                drop(view);
                drop(app);
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn owning_test_view_drains_profile_openers_before_namespace_restoration() {
        use crate::session::test_support::isolate_app_dir_at;
        let outer = tempfile::tempdir().unwrap();
        let _outer = isolate_app_dir_at(outer.path());
        let outer_app = crate::session::get_app_dir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let inner_guard = isolate_app_dir_at(inner.path());
        Storage::new_unwatched("inner").unwrap();
        let mut view = HomeView::new_for_test(
            Some("inner".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (written_tx, written_rx) = std::sync::mpsc::channel();
        view.drop_poller = crate::tui::worker::TrackedWorker::spawn(
            "aoe-test-owned-profile-opener",
            move |request: DropRequest| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let result = (|| -> anyhow::Result<bool> {
                    let storage = Storage::new_unwatched("late-worker")?;
                    storage.update(|rows, _| {
                        rows.push(request.instance.clone());
                        Ok(())
                    })?;
                    written_tx
                        .send(storage.sessions_path().to_path_buf())
                        .unwrap();
                    Ok(true)
                })();
                DropResult {
                    instance: request.instance,
                    dropped: result,
                }
            },
        );
        let finishing = view.drop_poller.observe_finish_for_test();
        let instance = Instance::new("worker", inner.path().to_str().unwrap());
        view.drop_poller
            .request(instance.id.clone(), DropRequest { instance });
        entered_rx.recv().unwrap();
        let release = std::thread::spawn(move || {
            let drain_started = finishing
                .recv_timeout(std::time::Duration::from_secs(3))
                .is_ok();
            release_tx.send(()).unwrap();
            drain_started
        });
        drop(view);
        drop(inner_guard);
        let drain_started = release.join().unwrap();
        let written = written_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert!(drain_started, "fixture did not drain the managed worker");
        assert!(written.starts_with(inner.path()));
        assert!(!outer_app.join(".workspace-claim.lock").exists());
        assert!(!outer_app.join("profiles").exists());
    }

    /// Construct a real trash lifecycle with a deterministic clock value.
    fn trashed_instance(trashed_at: chrono::DateTime<chrono::Utc>) -> Instance {
        let mut instance = Instance::new("trashed", "/tmp/trashed");
        instance.trash();
        instance.trashed_at = Some(trashed_at);
        instance
    }

    #[test]
    fn drop_retry_state_survives_failed_or_declined_removal() {
        let trashed_at = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
            .expect("fixed test timestamp is valid");
        let requested = trashed_instance(trashed_at);
        let current = requested.clone();

        assert!(!should_clear_drop_retry_state(
            &Err(anyhow::anyhow!("storage error")),
            &requested,
            Some(&current),
        ));
        assert!(!should_clear_drop_retry_state(
            &Ok(false),
            &requested,
            Some(&current),
        ));
    }

    #[test]
    fn drop_retry_state_clears_after_success_or_lifecycle_change() {
        let trashed_at = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
            .expect("fixed test timestamp is valid");
        let requested = trashed_instance(trashed_at);
        let current = requested.clone();
        let restored = Instance::new("trashed", "/tmp/trashed");
        let re_trashed = trashed_instance(trashed_at + chrono::Duration::seconds(1));

        assert!(should_clear_drop_retry_state(
            &Ok(true),
            &requested,
            Some(&current),
        ));
        assert!(should_clear_drop_retry_state(
            &Ok(false),
            &requested,
            Some(&restored),
        ));
        assert!(should_clear_drop_retry_state(
            &Err(anyhow::anyhow!("storage error")),
            &requested,
            Some(&re_trashed),
        ));
        assert!(should_clear_drop_retry_state(&Ok(false), &requested, None,));
    }

    #[test]
    fn drop_retry_state_invalidates_retrash_even_when_timestamps_collide() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let requested = trashed_instance(at);
        let mut peer: Instance =
            serde_json::from_str(&serde_json::to_string(&requested).unwrap()).unwrap();
        peer.untrash();
        peer.trash();
        peer.trashed_at = requested.trashed_at;
        let current: Instance =
            serde_json::from_str(&serde_json::to_string(&peer).unwrap()).unwrap();
        assert_ne!(current.trash_lifecycle_id, requested.trash_lifecycle_id);
        for dropped in [Ok(false), Err(anyhow::anyhow!("storage error"))] {
            assert!(should_clear_drop_retry_state(
                &dropped,
                &requested,
                Some(&current),
            ));
        }
        let mut unidentified = requested.clone();
        unidentified.trash_lifecycle_id = None;
        assert!(should_clear_drop_retry_state(
            &Ok(false),
            &unidentified,
            Some(&unidentified),
        ));
    }
}

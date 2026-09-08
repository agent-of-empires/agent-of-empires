//! Opening the dialogs the list view owns.

use super::*;

impl HomeView {
    pub fn show_intro(&mut self, current_theme: &str) {
        tracing::info!(target: "tui.dialog", dialog = "intro", "opening");
        self.intro_dialog = Some(IntroDialog::new(current_theme));
    }

    pub fn show_no_agents(&mut self) {
        tracing::info!(target: "tui.dialog", dialog = "no_agents", "opening");
        self.no_agents_dialog = Some(NoAgentsDialog::new());
    }

    /// Replace available tools (used after re-check from no-agents dialog).
    pub fn set_available_tools(&mut self, tools: AvailableTools) {
        tracing::debug!(target: "tui.home", count = tools.available_list().len(), "available tools refreshed");
        self.available_tools = tools;
    }

    pub fn show_changelog(&mut self, from_version: Option<String>) {
        tracing::info!(
            target: "tui.dialog",
            dialog = "changelog",
            from_version = ?from_version,
            "opening",
        );
        self.changelog_dialog = Some(ChangelogDialog::new(from_version));
    }

    pub fn show_telemetry_consent(&mut self) {
        tracing::info!(target: "tui.dialog", dialog = "telemetry_consent", "opening");
        self.telemetry_consent_dialog = Some(crate::tui::dialogs::TelemetryConsentDialog::new());
    }

    /// Show the profile picker dialog with fresh data from disk.
    pub(in crate::tui) fn show_profile_picker(&mut self) {
        use crate::session::list_profiles_for_display;
        use crate::tui::dialogs::{ProfileEntry, ProfilePickerDialog};

        let current_profile = self
            .active_profile
            .clone()
            .unwrap_or_else(|| "all".to_string());
        let profiles = list_profiles_for_display()
            .unwrap_or_else(|_| vec![crate::session::config::resolve_default_profile()]);
        let mut entries: Vec<ProfileEntry> = profiles
            .iter()
            .map(|name| {
                let session_count = Storage::new(name, self.file_watch.clone())
                    .and_then(|s| s.load())
                    .map(|instances| instances.len())
                    .unwrap_or(0);
                ProfileEntry {
                    name: name.clone(),
                    session_count,
                    is_active: self.active_profile.as_deref() == Some(name.as_str()),
                }
            })
            .collect();

        // In filtered mode, add "all" entry at top
        if self.active_profile.is_some() {
            let total: usize = entries.iter().map(|e| e.session_count).sum();
            entries.insert(
                0,
                ProfileEntry {
                    name: "all".to_string(),
                    session_count: total,
                    is_active: false,
                },
            );
        }

        self.profile_picker_dialog = Some(ProfilePickerDialog::new(entries, &current_profile));
    }

    /// Show the group-by picker dialog seeded with the current mode.
    pub(in crate::tui) fn show_group_picker(&mut self) {
        self.group_picker_dialog = Some(GroupPickerDialog::new(self.group_by));
    }

    /// Open the saved-project picker that starts a new session pre-filled with
    /// the chosen project's path. Opens the add-project form when none exist.
    pub(in crate::tui) fn open_project_session_picker(&mut self) {
        let profile = self.config_profile();
        match crate::session::projects::load_merged(&profile) {
            Ok(projects) if projects.is_empty() => {
                self.projects_dialog = Some(ProjectsDialog::new_adding(&profile));
            }
            Ok(projects) => {
                self.project_session_picker_dialog =
                    Some(ProjectSessionPickerDialog::new(projects));
            }
            Err(e) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Projects Failed",
                    &format!("Failed to load projects: {e}"),
                ));
            }
        }
    }

    /// Show the sort-order picker dialog seeded with the current order.
    pub(in crate::tui) fn show_sort_picker(&mut self) {
        self.sort_picker_dialog = Some(SortPickerDialog::new(self.sort_order));
    }

    /// Open the attach-a-project picker for the selected session (#3103).
    ///
    /// Offers registered projects minus the ones the session already has, which
    /// is the same rejection `session::attach_project` would apply anyway;
    /// filtering here means the user is not offered a choice that can only fail.
    pub(in crate::tui) fn open_add_project_for_selected(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        // Same lifecycle gate every sibling mutator applies. A row mid-create or
        // mid-delete must not gain a worktree, and a trashed or archived row's
        // agent is deliberately stopped, so attaching there would create a
        // worktree nothing is going to read. `for_session` offers the row
        // unconditionally, so the refusal has to live here.
        let shelved = self.get_instance(&id).and_then(|inst| {
            if inst.scratch {
                // No repo of its own to widen: a scratch session's cwd is a
                // throwaway directory under the app dir. `attach_project::plan`
                // refuses it too; catching it here means the picker never opens
                // on a session where every choice would fail.
                Some((
                    "Scratch Session",
                    "This is a scratch session, which has no repo to attach to. Create a session on the repo instead."
                        .to_string(),
                ))
            } else if matches!(
                inst.status,
                crate::session::Status::Deleting | crate::session::Status::Creating
            ) {
                Some((
                    "Session Busy",
                    "This session is still being created or is being deleted; wait for it to settle before attaching a project.".to_string(),
                ))
            } else if inst.status.blocks_worktree_edit() {
                // Attaching bounces the worker, which mid-turn would drop the
                // agent's reply, and `Waiting` is a turn in flight too: the agent
                // has paused on a question, so a SIGTERM here throws away a
                // pending approval. The daemon endpoint refuses on the
                // authoritative event-log probe (`has_in_flight_turn`); the TUI
                // has no handle on that store, so it reuses the status set
                // `blocks_worktree_edit` already encodes for exactly this reason
                // rather than keeping its own narrower copy.
                Some((
                    "Agent Working",
                    "This session's agent is mid-turn and attaching restarts it. Wait for the turn to finish, or stop the session first."
                        .to_string(),
                ))
            } else if inst.is_trashed() {
                Some((
                    "Session in Trash",
                    "This session is in the trash. Restore it before attaching a project."
                        .to_string(),
                ))
            } else if inst.is_archived() {
                Some((
                    "Session Archived",
                    "This session is archived and its agent stays stopped. Unarchive it before attaching a project."
                        .to_string(),
                ))
            } else {
                None
            }
        });
        if let Some((dialog_title, body)) = shelved {
            self.info_dialog = Some(InfoDialog::new(dialog_title, &body));
            return;
        }

        let Some((title, taken, profile)) = self.get_instance(&id).map(|inst| {
            let mut taken: Vec<String> = inst
                .all_repos()
                .iter()
                .map(|r| r.main_repo_path.clone())
                .collect();
            if let Some(wt) = inst.worktree_info.as_ref() {
                taken.push(wt.main_repo_path.clone());
            }
            taken.push(inst.project_path.clone());
            (
                inst.title.clone(),
                taken
                    .iter()
                    .map(|p| crate::session::projects::canonical_key(p))
                    .collect::<Vec<_>>(),
                // The session's own profile, not the view's filter: a session
                // belongs to one profile and its registry is that profile's.
                inst.source_profile.clone(),
            )
        }) else {
            return;
        };

        let options: Vec<crate::session::Project> = crate::session::projects::load_merged(&profile)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !taken.contains(&crate::session::projects::canonical_key(&p.path)))
            .collect();

        self.attach_project_dialog = Some(AttachProjectDialog::new(id, title, options));
    }

    /// Dispatch the attach for a picked project and say that it started.
    ///
    /// The work runs on `attach_project_poller`, so this returns immediately and
    /// the outcome replaces this dialog in `apply_attach_project_results`. A
    /// refusal that could be decided synchronously is reported as such.
    pub(in crate::tui) fn finish_add_project(
        &mut self,
        id: &str,
        project: &crate::session::Project,
    ) {
        match self.add_project_to_session(id, std::path::Path::new(&project.path)) {
            Ok(()) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Attaching Project",
                    &format!(
                        "Attaching '{}'. Creating the worktree can take a moment; this dialog \
                         updates when it finishes.",
                        project.name
                    ),
                ));
            }
            Err(e) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Could Not Attach Project",
                    &format!("{e:#}"),
                ));
            }
        }
    }

    /// Drain finished attaches, reload from disk and report each outcome.
    ///
    /// Returns true when anything landed, so the caller repaints. Both outcomes
    /// get a dialog rather than a transient toast: a success has consequences
    /// worth stating (the agent is restarting, or will only see the repo on next
    /// start), and a failure is usually the branch-already-exists refusal, which
    /// the user needs to read to know the CLI flag exists.
    pub fn apply_attach_project_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let mut touched = false;
        loop {
            match self.attach_project_poller.try_recv_result() {
                Ok(result) => {
                    self.attach_project_in_flight.remove(&result.session_id);
                    touched = true;
                    match result.outcome {
                        Ok(message) => {
                            // The worker persisted through `Storage`, so the
                            // in-memory list is stale until this reload. The disk
                            // watcher would get here on its own eventually; doing
                            // it now means the new repo is on the row by the time
                            // the success dialog is read.
                            if let Err(e) = self.reload() {
                                tracing::warn!(
                                    target: "session.attach",
                                    id = %result.session_id,
                                    "attach landed but the reload failed: {e:#}"
                                );
                            }
                            self.info_dialog = Some(InfoDialog::new("Project Attached", &message));
                        }
                        Err(message) => {
                            self.info_dialog =
                                Some(InfoDialog::new("Could Not Attach Project", &message));
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The worker thread is gone (a panic in the attach). Clearing
                    // the markers matters more than the lost result: otherwise
                    // every session it held stays permanently unattachable.
                    if !self.attach_project_in_flight.is_empty() {
                        tracing::error!(
                            target: "session.attach",
                            pending = self.attach_project_in_flight.len(),
                            "attach poller thread is gone; clearing in-flight markers"
                        );
                        self.attach_project_in_flight.clear();
                        touched = true;
                    }
                    break;
                }
            }
        }
        touched
    }
}

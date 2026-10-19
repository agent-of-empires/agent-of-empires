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

    pub(in crate::tui) fn show_profile_picker(&mut self) {
        use crate::tui::dialogs::ProfileEntry;

        let current_profile = self.active_profile.as_deref().unwrap_or("all");
        let profiles = crate::session::list_profiles_for_display()
            .unwrap_or_else(|_| vec![crate::session::config::resolve_default_profile()]);
        let mut entries: Vec<ProfileEntry> = profiles
            .into_iter()
            .map(|name| ProfileEntry {
                session_count: Storage::new(&name, self.file_watch.clone())
                    .and_then(|s| s.load())
                    .map_or(0, |instances| instances.len()),
                is_active: self.active_profile.as_deref() == Some(name.as_str()),
                name,
            })
            .collect();
        if self.active_profile.is_some() {
            let total = entries.iter().map(|e| e.session_count).sum();
            entries.insert(
                0,
                ProfileEntry {
                    name: "all".to_string(),
                    session_count: total,
                    is_active: false,
                },
            );
        }
        self.profile_picker_dialog = Some(ProfilePickerDialog::new(entries, current_profile));
    }

    pub(in crate::tui) fn show_group_picker(&mut self) {
        self.group_picker_dialog = Some(GroupPickerDialog::new(self.group_by));
    }

    /// Open the saved-project picker for a new session, or the add-project
    /// form when none exist.
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

    pub(in crate::tui) fn show_sort_picker(&mut self) {
        self.sort_picker_dialog = Some(SortPickerDialog::new(self.sort_order));
    }

    /// Open the attach-a-project picker for the selected session, offering
    /// only registered projects the session doesn't already have.
    pub(in crate::tui) fn open_add_project_for_selected(&mut self) {
        use crate::session::projects::canonical_key;
        use crate::session::Status;

        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&id) else {
            return;
        };
        // Refuse where every choice would fail or is unsafe: attaching restarts
        // the agent (a mid-turn or Waiting agent would lose its reply or
        // pending approval), and shelved rows keep their agent stopped.
        let refusal = if inst.scratch {
            Some(("Scratch Session", "This is a scratch session, which has no repo to attach to. Create a session on the repo instead."))
        } else if matches!(inst.status, Status::Deleting | Status::Creating) {
            Some(("Session Busy", "This session is still being created or is being deleted; wait for it to settle before attaching a project."))
        } else if inst.status.blocks_worktree_edit() {
            Some(("Agent Working", "This session's agent is mid-turn and attaching restarts it. Wait for the turn to finish, or stop the session first."))
        } else if inst.is_trashed() {
            Some((
                "Session in Trash",
                "This session is in the trash. Restore it before attaching a project.",
            ))
        } else if inst.is_archived() {
            Some(("Session Archived", "This session is archived and its agent stays stopped. Unarchive it before attaching a project."))
        } else {
            None
        };
        if let Some((dialog_title, body)) = refusal {
            self.info_dialog = Some(InfoDialog::new(dialog_title, body));
            return;
        }

        let taken: Vec<String> = inst
            .all_repos()
            .iter()
            .map(|r| r.main_repo_path.as_str())
            .chain(
                inst.worktree_info
                    .as_ref()
                    .map(|wt| wt.main_repo_path.as_str()),
            )
            .chain(std::iter::once(inst.project_path.as_str()))
            .map(canonical_key)
            .collect();
        let title = inst.title.clone();
        // The session's own profile, not the view's filter.
        let options: Vec<crate::session::Project> =
            crate::session::projects::load_merged(&inst.source_profile)
                .unwrap_or_default()
                .into_iter()
                .filter(|p| !taken.contains(&canonical_key(&p.path)))
                .collect();

        self.attach_project_dialog = Some(AttachProjectDialog::new(id, title, options));
    }

    /// Dispatch the attach on `attach_project_poller`; the outcome replaces
    /// this dialog in `apply_attach_project_results`.
    pub(in crate::tui) fn finish_add_project(
        &mut self,
        id: &str,
        project: &crate::session::Project,
    ) {
        self.info_dialog = Some(
            match self.add_project_to_session(id, std::path::Path::new(&project.path)) {
                Ok(()) => InfoDialog::new(
                    "Attaching Project",
                    &format!(
                        "Attaching '{}'. Creating the worktree can take a moment; this dialog \
                         updates when it finishes.",
                        project.name
                    ),
                ),
                Err(e) => InfoDialog::new("Could Not Attach Project", &format!("{e:#}")),
            },
        );
    }

    /// Drain finished attaches, reload from disk and report each outcome in
    /// a dialog. Returns true when anything landed.
    pub fn apply_attach_project_results(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let mut touched = false;
        loop {
            match self.attach_project_poller.try_recv_result() {
                Ok(result) => {
                    self.attach_project_in_flight.remove(&result.session_id);
                    touched = true;
                    self.info_dialog = Some(match result.outcome {
                        Ok(message) => {
                            // Reload now so the new repo is on the row when the dialog is read.
                            if let Err(e) = self.reload() {
                                tracing::warn!(
                                    target: "session.attach",
                                    id = %result.session_id,
                                    "attach landed but the reload failed: {e:#}"
                                );
                            }
                            InfoDialog::new("Project Attached", &message)
                        }
                        Err(message) => InfoDialog::new("Could Not Attach Project", &message),
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Worker panicked: clear markers or those sessions stay unattachable.
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

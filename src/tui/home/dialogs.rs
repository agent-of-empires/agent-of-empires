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
        use crate::tui::dialogs::{ProfileEntry, ProfilePickerDialog};
        let current_profile = self
            .active_profile
            .clone()
            .unwrap_or_else(|| "all".to_owned());
        let canonical = self.session_feed.applied_snapshot();
        let mut entries: Vec<ProfileEntry> = if let Some(snapshot) = canonical.as_ref() {
            snapshot
                .contents
                .profiles
                .iter()
                .map(|profile| ProfileEntry {
                    name: profile.name.clone(),
                    session_count: snapshot
                        .contents
                        .sessions
                        .iter()
                        .filter(|row| row.profile == profile.name)
                        .count(),
                    is_active: self.active_profile.as_deref() == Some(profile.name.as_str()),
                })
                .collect()
        } else {
            let entries = (|| -> anyhow::Result<Vec<ProfileEntry>> {
                crate::session::list_profiles_for_display()?
                    .into_iter()
                    .map(|name| {
                        let storage = Storage::open(&name, self.file_watch.clone())?;
                        Ok(ProfileEntry {
                            session_count: storage.load_complete_with_groups()?.0.len(),
                            is_active: self.active_profile.as_deref() == Some(name.as_str()),
                            name,
                        })
                    })
                    .collect()
            })();
            match entries {
                Ok(entries) => entries,
                Err(error) => {
                    self.info_dialog =
                        Some(InfoDialog::new("Cannot Read Profiles", &error.to_string()));
                    return;
                }
            }
        };
        if canonical.is_some() {
            entries.sort_by(|left, right| {
                (left.name == "default", &left.name).cmp(&(right.name == "default", &right.name))
            });
        }
        if self.active_profile.is_some() {
            entries.insert(
                0,
                ProfileEntry {
                    name: "all".to_owned(),
                    session_count: entries.iter().map(|entry| entry.session_count).sum(),
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
                Some((
                    "Scratch Session",
                    "This is a scratch session, which has no repo to attach to. Create a session on the repo instead.",
                ))
            } else if matches!(
                inst.status,
                crate::session::Status::Deleting | crate::session::Status::Creating
            ) {
                Some((
                    "Session Busy",
                    "This session is still being created or is being deleted; wait for it to settle before attaching a project.",
                ))
            } else if inst.status.blocks_worktree_edit() {
                // The status gate mirrors the daemon’s in-flight-turn refusal.
                Some((
                    "Agent Working",
                    "This session's agent is mid-turn and attaching restarts it. Wait for the turn to finish, or stop the session first.",
                ))
            } else if inst.is_trashed() {
                Some((
                    "Session in Trash",
                    "This session is in the trash. Restore it before attaching a project.",
                ))
            } else if inst.is_archived() {
                Some((
                    "Session Archived",
                    "This session is archived and its agent stays stopped. Unarchive it before attaching a project.",
                ))
            } else {
                None
            }
        });
        if let Some((dialog_title, body)) = shelved {
            self.info_dialog = Some(InfoDialog::new(dialog_title, body));
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
                    .map(crate::session::projects::canonical_key)
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
        self.info_dialog = Some(
            match self.add_project_to_session(id, std::path::Path::new(&project.path)) {
                Ok(()) => InfoDialog::new(
                    "Attaching Project",
                    &format!(
                        "Attaching '{}'. Creating the worktree can take a moment; this dialog \
                     updates when it finishes.",
                        project.name,
                    ),
                ),
                Err(e) => InfoDialog::new("Could Not Attach Project", &format!("{e:#}")),
            },
        );
    }
}

//! Pane sizes, collapsed sections, and the app state that persists them.

use super::*;

impl HomeView {
    pub fn shrink_list(&mut self) {
        self.list_width = self.list_width.saturating_sub(5).max(10);
        self.save_list_width();
    }

    pub fn grow_list(&mut self) {
        self.list_width = (self.list_width + 5).min(80);
        self.save_list_width();
    }

    /// Collapse the session list to a narrow click-to-expand strip, or
    /// re-expand it. The next render reflows the preview into the freed
    /// width; in live mode the resize loop pushes the new geometry to the
    /// agent's pane. Persisted so the choice survives restarts.
    pub fn toggle_sidebar_collapsed(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.save_sidebar_collapsed();
    }

    /// Apply `mutate` to `state.toml`'s `AppStateConfig` and write it back. The
    /// failure path is logged, so a UI-preference write never fails silently.
    /// Centralizes the load/mutate/save boilerplate the home view's preference
    /// persisters would otherwise each repeat.
    fn persist_app_state(
        what: &str,
        mutate: impl FnOnce(&mut crate::session::config::AppStateConfig),
    ) {
        if let Err(e) = update_app_state(mutate) {
            tracing::warn!(target: "tui.home", "Failed to save app state ({what}): {e}");
        }
    }

    fn save_sidebar_collapsed(&self) {
        let collapsed = self.sidebar_collapsed;
        Self::persist_app_state("sidebar collapsed", |s| {
            s.home_sidebar_collapsed = Some(collapsed)
        });
    }

    pub(super) fn save_list_width(&self) {
        let width = self.list_width;
        Self::persist_app_state("list width", |s| s.home_list_width = Some(width));
    }

    /// Folder paths that currently map to a real project-mode folder: the
    /// project group of every live session (across all profiles, so switching
    /// the active profile never prunes another profile's collapse state),
    /// registered empty projects, and the per-project archived sub-folders.
    /// Used to drop collapse entries for projects that no longer exist.
    fn known_project_group_paths(&self) -> std::collections::HashSet<String> {
        let mut paths = std::collections::HashSet::new();
        for inst in self.instances.values() {
            let group = project_group_key(inst);
            if group.is_empty() {
                continue;
            }
            if inst.is_archived() {
                paths.insert(crate::session::archived_project_sub_path(&group));
            } else {
                paths.insert(group);
            }
        }
        for project in &self.registered_projects {
            // Only pinned registered projects surface as empty folder headers,
            // keyed by repo label (mirroring `unpopulated_projects`).
            if project.pinned {
                paths.insert(crate::session::projects::repo_label(&project.path));
            }
        }
        paths
    }

    /// Persist the set of collapsed project-mode folders. Stored sorted for a
    /// stable on-disk order; only collapsed paths are written, so re-expanding a
    /// folder drops it from the list. Paths for projects that no longer exist
    /// are pruned here so the persisted set can't grow without bound as projects
    /// come and go.
    pub(super) fn save_project_group_collapsed(&self) {
        let known = self.known_project_group_paths();
        let mut collapsed: Vec<String> = self
            .project_group_collapsed
            .iter()
            .filter(|(path, &c)| c && known.contains(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect();
        collapsed.sort();
        Self::persist_app_state("project group collapsed", |s| {
            s.project_group_collapsed = collapsed
        });
    }

    /// Org-mode counterpart of `known_project_group_paths`: the org group of
    /// every live session (across all profiles) and the per-org archived
    /// sub-folders. No registered-projects loop: org headers have no
    /// registry to surface empty "pinned" headers from (#3283 out of scope).
    fn known_org_group_paths(&self) -> std::collections::HashSet<String> {
        let mut paths = std::collections::HashSet::new();
        for inst in self.instances.values() {
            let group = self.org_group_key(inst);
            if group.is_empty() {
                continue;
            }
            if inst.is_archived() {
                paths.insert(crate::session::archived_project_sub_path(&group));
            } else {
                paths.insert(group);
            }
        }
        paths
    }

    /// Persist the set of collapsed org-mode folders. Same shape and pruning
    /// rationale as `save_project_group_collapsed`.
    pub(super) fn save_org_group_collapsed(&self) {
        let known = self.known_org_group_paths();
        let mut collapsed: Vec<String> = self
            .org_group_collapsed
            .iter()
            .filter(|(path, &c)| c && known.contains(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect();
        collapsed.sort();
        Self::persist_app_state("org group collapsed", |s| s.org_group_collapsed = collapsed);
    }

    pub fn toggle_preview_info(&mut self) {
        self.show_preview_info = !self.show_preview_info;
        let show = self.show_preview_info;
        Self::persist_app_state("preview info", |s| s.show_preview_info = Some(show));
    }

    /// Forget one session's passive-resize bookkeeping so the next render
    /// re-asserts its preview geometry. Call whenever that agent window's real
    /// size changes out from under the preview (an attach grows it to the
    /// client; entering or leaving live mode hands the resize off and back).
    pub(in crate::tui) fn clear_preview_pane_sync(&mut self, session_id: &str) {
        self.passive_pane_synced.remove(session_id);
        self.passive_pane_declined.remove(session_id);
        self.passive_pane_queued.remove(session_id);
    }

    /// Expand the synthetic Archived section if it is collapsed, persisting
    /// the change. Used when archiving a whole group, where the rows the
    /// user was looking at all sink at once and revealing the section shows
    /// where they went. Single-row archive does NOT reveal: the cursor
    /// advances to the next active session instead and the section header's
    /// count is the feedback. No-op (and no save) when already open.
    pub(in crate::tui) fn reveal_archived_section(&mut self) {
        if !self.archived_section_collapsed {
            return;
        }
        self.archived_section_collapsed = false;
        Self::persist_app_state("archived section", |s| {
            s.archived_section_collapsed = Some(false)
        });
    }

    pub fn toggle_trashed_section(&mut self) {
        self.trashed_section_collapsed = !self.trashed_section_collapsed;
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }

    pub fn toggle_archived_section(&mut self) {
        self.archived_section_collapsed = !self.archived_section_collapsed;
        let collapsed = self.archived_section_collapsed;
        Self::persist_app_state("archived section", |s| {
            s.archived_section_collapsed = Some(collapsed)
        });
        self.rebuild_flat_items();
        // Defensive cursor clamp + selection refresh. Today the only
        // call site routes through `toggle_group_collapsed` after the
        // cursor lands on the section header, and the header survives
        // the rebuild at the same end-of-list index, so the cursor stays
        // valid. Programmatic callers (palette command, future macros)
        // wouldn't have that invariant, so clamp here rather than rely
        // on every caller to know about it.
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }
}

//! Pane sizes, collapsed sections, and the app state that persists them.

use std::collections::HashSet;

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

    pub fn toggle_sidebar_collapsed(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        let collapsed = self.sidebar_collapsed;
        Self::persist_app_state("sidebar collapsed", |s| {
            s.home_sidebar_collapsed = Some(collapsed)
        });
    }

    fn persist_app_state(
        what: &str,
        mutate: impl FnOnce(&mut crate::session::config::AppStateConfig),
    ) {
        if let Err(e) = update_app_state(mutate) {
            tracing::warn!(target: "tui.home", "Failed to save app state ({what}): {e}");
        }
    }

    pub(super) fn save_list_width(&self) {
        let width = self.list_width;
        Self::persist_app_state("list width", |s| s.home_list_width = Some(width));
    }

    /// Group paths that currently exist under `key`, across all profiles so
    /// switching profile never prunes another profile's collapse state.
    fn known_group_paths(&self, key: impl Fn(&Instance) -> String) -> HashSet<String> {
        self.instances
            .values()
            .filter_map(|inst| {
                let group = key(inst);
                if group.is_empty() {
                    None
                } else if inst.is_archived() {
                    Some(crate::session::archived_project_sub_path(&group))
                } else {
                    Some(group)
                }
            })
            .collect()
    }

    /// Sorted collapsed paths, pruned to `known` so the persisted set can't grow unbounded.
    fn collapsed_known_paths(
        collapsed: &HashMap<String, bool>,
        known: HashSet<String>,
    ) -> Vec<String> {
        let mut paths: Vec<String> = collapsed
            .iter()
            .filter(|(path, &c)| c && known.contains(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect();
        paths.sort();
        paths
    }

    pub(super) fn save_project_group_collapsed(&self) {
        let mut known = self.known_group_paths(project_group_key);
        // Pinned registered projects surface as empty headers keyed by repo label.
        known.extend(
            self.registered_projects
                .iter()
                .filter(|p| p.pinned)
                .map(|p| crate::session::projects::repo_label(&p.path)),
        );
        let collapsed = Self::collapsed_known_paths(&self.project_group_collapsed, known);
        Self::persist_app_state("project group collapsed", |s| {
            s.project_group_collapsed = collapsed
        });
    }

    pub(super) fn save_org_group_collapsed(&self) {
        let known = self.known_group_paths(|inst| self.org_group_key(inst));
        let collapsed = Self::collapsed_known_paths(&self.org_group_collapsed, known);
        Self::persist_app_state("org group collapsed", |s| s.org_group_collapsed = collapsed);
    }

    pub fn toggle_preview_info(&mut self) {
        self.show_preview_info = !self.show_preview_info;
        let show = self.show_preview_info;
        Self::persist_app_state("preview info", |s| s.show_preview_info = Some(show));
    }

    /// Forget one session's passive-resize bookkeeping so the next render
    /// re-asserts its preview geometry. Call whenever the agent window's real
    /// size changes out from under the preview (attach, live mode enter/exit).
    pub(in crate::tui) fn clear_preview_pane_sync(&mut self, session_id: &str) {
        self.passive_pane_synced.remove(session_id);
        self.passive_pane_declined.remove(session_id);
        self.passive_pane_queued.remove(session_id);
    }

    /// Expand the Archived section if collapsed, for group archives where
    /// every row the user was looking at sinks at once.
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
        self.rebuild_and_clamp_cursor();
    }

    pub fn toggle_archived_section(&mut self) {
        self.archived_section_collapsed = !self.archived_section_collapsed;
        let collapsed = self.archived_section_collapsed;
        Self::persist_app_state("archived section", |s| {
            s.archived_section_collapsed = Some(collapsed)
        });
        self.rebuild_and_clamp_cursor();
    }

    fn rebuild_and_clamp_cursor(&mut self) {
        self.rebuild_flat_items();
        if !self.flat_items.is_empty() && self.cursor >= self.flat_items.len() {
            self.cursor = self.flat_items.len() - 1;
        }
        self.update_selected();
    }
}

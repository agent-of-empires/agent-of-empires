//! Moving and revealing the cursor.

use super::*;

impl HomeView {
    fn session_row(&self, session_id: &str) -> Option<usize> {
        self.flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
    }

    pub fn select_session_by_id(&mut self, session_id: &str) {
        if let Some(idx) = self.session_row(session_id) {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Rebuild `flat_items` and move the cursor back onto the selected session or group.
    pub(super) fn rebuild_flat_items_keeping_cursor(&mut self) {
        self.rebuild_flat_items();
        let restored = match (&self.selected_session, &self.selected_group) {
            (Some(sid), _) => self.session_row(sid),
            (None, Some(gpath)) => self
                .flat_items
                .iter()
                .position(|item| matches!(item, Item::Group { path, .. } if path == gpath)),
            (None, None) => None,
        };
        match restored {
            Some(idx) => self.cursor = idx,
            None if self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() => {
                self.cursor = self.flat_items.len() - 1;
            }
            None => {}
        }
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    /// Move the cursor to the first session row, skipping `returning_id` unless it is the only one.
    pub fn select_top_attention(&mut self, returning_id: Option<&str>) {
        let mut sessions =
            self.flat_items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| match item {
                    Item::Session { id, .. } => Some((idx, returning_id == Some(id.as_str()))),
                    _ => None,
                });
        let first = sessions.next();
        let pick = match first {
            Some((_, true)) => sessions.find(|(_, returning)| !returning).or(first),
            other => other,
        };
        if let Some((idx, _)) = pick {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Select `session_id`, expanding its collapsed group so the row exists.
    /// No-op when the session is gone.
    pub fn select_and_reveal_session(&mut self, session_id: &str) {
        let Some(inst) = self.get_instance(session_id) else {
            return;
        };
        let target_profile = inst.source_profile.clone();
        let group_path = match self.group_by {
            GroupByMode::Project => Some(project_group_key(inst)),
            GroupByMode::Org => Some(self.org_group_key(inst)),
            GroupByMode::Manual => Some(inst.group_path.clone()).filter(|p| !p.is_empty()),
        };
        self.selected_session = Some(session_id.to_string());
        self.selected_group = None;
        self.selected_group_profile = None;
        if let Some(gpath) = group_path {
            match self.group_by {
                GroupByMode::Project => {
                    self.project_group_collapsed.insert(gpath, false);
                }
                GroupByMode::Org => {
                    self.org_group_collapsed.insert(gpath, false);
                }
                GroupByMode::Manual => {
                    if let Some(tree) = self.group_trees.get_mut(&target_profile) {
                        tree.set_collapsed(&gpath, false);
                    }
                }
            }
            self.rebuild_flat_items();
        }
        if let Some(pos) = self.session_row(session_id) {
            self.cursor = pos;
        }
    }
}

//! Moving and revealing the cursor.

use super::*;

impl HomeView {
    pub fn select_session_by_id(&mut self, session_id: &str) {
        for (idx, item) in self.flat_items.iter().enumerate() {
            if let Item::Session { id, .. } = item {
                if id == session_id {
                    self.cursor = idx;
                    self.update_selected();
                    return;
                }
            }
        }
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    /// Move the cursor to the highest-priority session row, skipping
    /// `returning_id` if provided. Used after returning from an attach while
    /// sort_order=Attention: `stamp_last_accessed` bumps the returning session
    /// to the top of its tier, so picking row 0 blindly would leave the cursor
    /// on the session the user just handled. Skip it and land on the next
    /// session that actually needs attention. Falls back to the returning
    /// session itself if it's the only one in the list.
    pub fn select_top_attention(&mut self, returning_id: Option<&str>) {
        let mut fallback: Option<usize> = None;
        for (idx, item) in self.flat_items.iter().enumerate() {
            if let Item::Session { id, .. } = item {
                if returning_id.is_some_and(|r| r == id) {
                    fallback.get_or_insert(idx);
                    continue;
                }
                self.cursor = idx;
                self.update_selected();
                return;
            }
        }
        if let Some(idx) = fallback {
            self.cursor = idx;
            self.update_selected();
        }
    }

    /// Pin selection to `session_id` and place the cursor on its row.
    /// If the containing group is collapsed (manual, project, or
    /// org grouping), it's force-expanded and `flat_items` is
    /// rebuilt so the row is actually present before the cursor
    /// search. No-op when the session can't be resolved at all
    /// (deleted between caller and us): leaves the prior selection
    /// untouched so the user doesn't see the cursor leap to nowhere.
    ///
    /// Used by `apply_creation_results` so a freshly-created session
    /// becomes the visible cursor row; also a natural fit for any
    /// future "jump to session" path (command palette deep link,
    /// API-driven focus change) that wants the same reveal behavior.
    pub fn select_and_reveal_session(&mut self, session_id: &str) {
        let Some(inst) = self.get_instance(session_id) else {
            return;
        };
        let group_path = match self.group_by {
            GroupByMode::Project => Some(project_group_key(inst)),
            GroupByMode::Org => Some(self.org_group_key(inst)),
            GroupByMode::Manual => {
                let p = inst.group_path.clone();
                if p.is_empty() {
                    None
                } else {
                    Some(p)
                }
            }
        };
        let target_profile = inst.source_profile.clone();
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
        if let Some(pos) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == session_id))
        {
            self.cursor = pos;
        }
    }
}

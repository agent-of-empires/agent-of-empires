//! Manual row ordering for the home list: move the cursor's session within its group, or its
//! group among the group's siblings. Only meaningful under [`SortOrder::Custom`] and
//! [`GroupByMode::Manual`], where the rows carry the user's own grouping and nothing
//! recomputes their order behind them.

use super::*;
use crate::session::config::{GroupByMode, SortOrder};

impl HomeView {
    /// Move the cursor's row one slot in `delta` (-1 up, 1 down). A session moves among the
    /// sessions of its own group and profile; a group header moves among the groups sharing
    /// its parent. Refuses where a move would be discarded or would write a membership the
    /// list is not showing.
    pub(super) fn move_row_at_cursor(
        &mut self,
        delta: isize,
    ) -> anyhow::Result<super::TransactionDisposition> {
        if self.sort_order != SortOrder::Custom {
            self.flash_status("Press o for the Custom sort to arrange rows by hand");
            return Ok(super::TransactionDisposition::Ignored);
        }
        if self.group_by != GroupByMode::Manual {
            // Project and Org headers are derived from repo paths, and their rows carry a
            // rewritten `group_path` for display. Moving one would save a manual membership
            // the user cannot see, so the mutation is refused rather than guessed at.
            self.flash_status("Rows are arranged by hand in Manual grouping only (g)");
            return Ok(super::TransactionDisposition::Ignored);
        }
        if let Some(group_path) = self.selected_group.clone() {
            return self.move_group_row(&group_path, delta);
        } else if let Some(id) = self.selected_session.clone() {
            if self.hide_stopped_in_groups {
                // A session move renumbers every sibling in the store, the hidden ones too.
                self.flash_status("Show stopped sessions (y) to move sessions by hand");
                return Ok(super::TransactionDisposition::Ignored);
            }
            return self.move_session_row(&id, delta);
        }
        Ok(super::TransactionDisposition::Ignored)
    }

    fn neighbouring_group(&self, id: &str, delta: isize) -> Option<String> {
        let row = self.get_instance(id)?;
        let groups = self.displayed_group_order(Some(&row.source_profile));
        let at = groups.iter().position(|g| *g == row.group_path)?;
        at.checked_add_signed(delta)
            .and_then(|to| groups.get(to))
            .cloned()
    }
    fn move_session_row(
        &mut self,
        id: &str,
        delta: isize,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let neighbour = self.neighbouring_group(id, delta);
        let row = self.capture_transaction_row(id)?;
        self.request_transaction(persistence_transactions::TransactionRequest::Ordering(
            persistence_transactions::OrderingRequest::Row {
                row,
                delta,
                direct: None,
                neighbour,
            },
        ))
    }
    /// ungrouped bucket that `flatten_tree` puts first. The synthetic Archived and Trash
    /// sections are left out: they are sinks a row reaches by being archived or trashed,
    /// never by being moved.
    fn displayed_group_order(&self, profile: Option<&str>) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for item in &self.flat_items {
            let path = match item {
                Item::Group {
                    path,
                    profile: row_profile,
                    ..
                } => {
                    // Single-profile flattening leaves a header's profile unset, so an
                    // unqualified row belongs to the view's only profile; comparing it
                    // against `Some(profile)` would drop every header and leave only the
                    // groups recovered from visible sessions, skipping empty or collapsed
                    // neighbours.
                    match (profile, row_profile.as_deref()) {
                        (Some(want), Some(have)) if want != have => continue,
                        _ => path.clone(),
                    }
                }
                Item::Session { id, .. } => match self.get_instance(id) {
                    Some(inst)
                        if !inst.is_archived()
                            && !inst.is_trashed()
                            && profile.is_none_or(|p| inst.source_profile == p) =>
                    {
                        inst.group_path.clone()
                    }
                    _ => continue,
                },
            };
            if crate::session::is_within_archived_section(&path)
                || crate::session::is_within_trash_section(&path)
            {
                continue;
            }
            if !order.contains(&path) {
                order.push(path);
            }
        }
        order
    }

    /// whatever the expansion write did, so the list is rebuilt either way: returning early on
    /// that error would leave the cursor on a row the store places elsewhere.
    pub(super) fn after_committed_cross_group_move(
        &mut self,
        id: &str,
        target: &str,
        revealed: anyhow::Result<()>,
    ) {
        self.rebuild_flat_items_keeping_cursor();
        let Err(error) = revealed else {
            return;
        };
        tracing::warn!(
            target: "tui.reorder",
            error = %error,
            "expanding the destination group failed after a move"
        );
        self.flash_status("Moved, but the destination group stayed collapsed");
        // The row went into a group that never opened, so the selection is on a session the
        // list is not drawing: follow it as far as the header it went under.
        if self
            .flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id: row, .. } if row == id))
        {
            return;
        }
        let row_profile = self.instances.get(id).map(|row| row.source_profile.clone());
        if let Some(at) = self.flat_items.iter().position(|item| {
            matches!(item, Item::Group { path, profile, .. }
                if path == target
                    && profile.as_deref().is_none_or(|header| Some(header) == row_profile.as_deref()))
        }) {
            self.cursor = at;
            self.update_selected();
        }
    }

    /// The one profile in play, when there is only one. A unified view over a single profile
    /// draws its headers unqualified, and an empty group has no member session to infer an
    /// owner from, so without this a stored group with no rows could not be moved at all.
    fn sole_storage_profile(&self) -> Option<String> {
        match self.storages.len() {
            1 => self.storages.keys().next().cloned(),
            _ => None,
        }
    }

    fn move_group_row(
        &mut self,
        group_path: &str,
        delta: isize,
    ) -> anyhow::Result<super::TransactionDisposition> {
        let Some(profile) = self
            .selected_group_profile
            .clone()
            .or_else(|| self.active_profile.clone())
            .or_else(|| self.sole_storage_profile())
        else {
            return Ok(super::TransactionDisposition::Ignored);
        };
        let storage = self
            .storages
            .get(&profile)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Original group profile is unavailable"))?;
        let collapsed = self
            .group_trees
            .get(&profile)
            .map(|t| {
                t.get_all_groups()
                    .into_iter()
                    .map(|g| (g.path, g.collapsed))
                    .collect()
            })
            .unwrap_or_default();
        self.request_transaction(persistence_transactions::TransactionRequest::Ordering(
            persistence_transactions::OrderingRequest::Group {
                storage,
                path: group_path.to_owned(),
                delta,
                collapsed,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::persistence_transactions::group_contains;

    /// Membership is decided on whole path components: a session in `ab` does not keep a
    /// deleted `a` alive, while one in `a/b` does.
    #[test]
    fn a_group_contains_its_members_and_its_descendants_only() {
        assert!(group_contains("a", "a"));
        assert!(group_contains("a", "a/b"));
        assert!(group_contains("a", "a/b/c"));
        assert!(group_contains("a/b", "a/b/c"));
        assert!(
            !group_contains("a", "ab"),
            "a shared prefix is not a parent"
        );
        assert!(!group_contains("a/b", "a"), "an ancestor is not a member");
        assert!(!group_contains("a/b", "a/bc"));
    }
}

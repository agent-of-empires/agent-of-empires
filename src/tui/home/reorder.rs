//! Manual row ordering for the home list: move the cursor's session within its group, or its
//! group among the group's siblings. Only meaningful under [`SortOrder::Custom`] and
//! [`GroupByMode::Manual`], where the rows carry the user's own grouping and nothing
//! recomputes their order behind them.

use super::*;
use crate::session::config::{GroupByMode, SortOrder};

/// Whether `member` is `group` or sits beneath it, compared on whole path components so `ab` is
/// not taken for a child of `a`. A group with no stored row still exists while a live session sits
/// in it or anywhere under it, because the tree draws every ancestor of a member's path.
fn group_contains(group: &str, member: &str) -> bool {
    member == group
        || member
            .strip_prefix(group)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// What a move attempt found once it held the lock.
enum MoveOutcome {
    /// The rows were renumbered and the view reconciled with them.
    Moved,
    /// The row is already against the edge of its group, so a caller may cross the boundary.
    AtEdge,
    /// The rows this move was aimed at have changed underneath the list: the anchor is gone,
    /// a peer moved it to another group, or the destination group no longer exists.
    Stale,
}

/// The same three answers for a group header, carrying the reordered groups when there are
/// any to publish.
enum GroupMoveOutcome {
    Moved(Vec<Group>),
    AtEdge,
    Stale,
}

impl HomeView {
    /// Move the cursor's row one slot in `delta` (-1 up, 1 down). A session moves among the
    /// sessions of its own group and profile; a group header moves among the groups sharing
    /// its parent. Refuses where a move would be discarded or would write a membership the
    /// list is not showing.
    pub(super) fn move_row_at_cursor(&mut self, delta: isize) -> anyhow::Result<()> {
        if self.sort_order != SortOrder::Custom {
            self.flash_status("Press o for the Custom sort to arrange rows by hand");
            return Ok(());
        }
        if self.group_by != GroupByMode::Manual {
            // Project and Org headers are derived from repo paths, and their rows carry a
            // rewritten `group_path` for display. Moving one would save a manual membership
            // the user cannot see, so the mutation is refused rather than guessed at.
            self.flash_status("Rows are arranged by hand in Manual grouping only (g)");
            return Ok(());
        }
        if let Some(group_path) = self.selected_group.clone() {
            self.move_group_row(&group_path, delta)?;
        } else if let Some(id) = self.selected_session.clone() {
            if self.hide_stopped_in_groups {
                // A session move renumbers every sibling in the store, the hidden ones too.
                self.flash_status("Show stopped sessions (y) to move sessions by hand");
                return Ok(());
            }
            self.move_session_row(&id, delta)?;
        }
        Ok(())
    }

    fn move_session_row(&mut self, id: &str, delta: isize) -> anyhow::Result<()> {
        match self.apply_move(id, delta, None)? {
            MoveOutcome::Moved => {
                self.rebuild_flat_items_keeping_cursor();
                Ok(())
            }
            // Already at the edge of its own group: carry on into the neighbouring one.
            MoveOutcome::AtEdge => self.move_session_across_groups(id, delta),
            MoveOutcome::Stale => self.refresh_after_stale_move(),
        }
    }

    /// Drop a move aimed at rows that have since changed and show the current list instead.
    /// Repeating the keystroke then acts on what is on screen.
    fn refresh_after_stale_move(&mut self) -> anyhow::Result<()> {
        self.reload_storage_only()?;
        self.flash_status("Rows changed elsewhere, list refreshed");
        Ok(())
    }

    /// Move `id` by `delta` inside its group, or into `target_group` when crossing a
    /// boundary, and renumber the whole destination sibling set — all computed from the rows
    /// the store holds under the lock. Reading the order beforehand and writing a diff lets a
    /// peer's concurrent swap survive alongside this one, leaving two rows sharing an index.
    /// The anchor and the destination are checked against those same locked rows, so a move
    /// decided from the drawn list cannot write a membership a peer has since removed.
    fn apply_move(
        &mut self,
        id: &str,
        delta: isize,
        target_group: Option<String>,
    ) -> anyhow::Result<MoveOutcome> {
        let Some(anchor) = self.instances.get(id) else {
            return Ok(MoveOutcome::Stale);
        };
        let profile = anchor.source_profile.clone();
        let source_group = anchor.group_path.clone();
        let Some(storage) = self.storages.get(&profile) else {
            return Ok(MoveOutcome::Stale);
        };

        let moved: Option<Vec<(String, u32, Option<String>)>> =
            storage.update(|instances, groups| {
                // The drawn row is a copy taken before the lock. If the store's row has since been
                // archived, trashed, deleted or regrouped, every position computed from the list is
                // about a layout that no longer exists.
                let anchor_is_current = instances.iter().any(|i| {
                    i.id == id
                        && i.source_profile == profile
                        && i.group_path == source_group
                        && !i.is_archived()
                        && !i.is_trashed()
                });
                if !anchor_is_current {
                    return Ok(None);
                }
                if let Some(destination) = &target_group {
                    // The ungrouped bucket is always there; any other destination has to be a
                    // group the store still holds, either declared or standing for live members.
                    // Writing the membership blind would let the next tree rebuild synthesize a
                    // group a peer has just deleted.
                    let destination_exists = destination.is_empty()
                        || groups.iter().any(|g| g.path == *destination)
                        || instances.iter().any(|i| {
                            group_contains(destination, &i.group_path)
                                && i.source_profile == profile
                                && !i.is_archived()
                                && !i.is_trashed()
                        });
                    if !destination_exists {
                        return Ok(None);
                    }
                }
                let group = target_group.clone().unwrap_or_else(|| source_group.clone());
                let mut siblings: Vec<&Instance> = instances
                    .iter()
                    .filter(|i| {
                        i.group_path == group
                            && i.source_profile == profile
                            && !i.is_archived()
                            && !i.is_trashed()
                            && i.id != id
                    })
                    .collect();
                siblings.sort_by_key(|i| {
                    (
                        i.sort_index.unwrap_or(u32::MAX),
                        std::cmp::Reverse(i.created_at),
                    )
                });
                let mut order: Vec<String> = siblings.iter().map(|i| i.id.to_string()).collect();

                match &target_group {
                    // Crossing a boundary: land against the edge that was crossed.
                    Some(_) if delta < 0 => order.push(id.to_string()),
                    Some(_) => order.insert(0, id.to_string()),
                    None => {
                        // Within the group, the anchor's own position is read under the lock too.
                        let mut own = instances
                            .iter()
                            .filter(|i| {
                                i.group_path == group
                                    && i.source_profile == profile
                                    && !i.is_archived()
                                    && !i.is_trashed()
                            })
                            .collect::<Vec<_>>();
                        own.sort_by_key(|i| {
                            (
                                i.sort_index.unwrap_or(u32::MAX),
                                std::cmp::Reverse(i.created_at),
                            )
                        });
                        let Some(at) = own.iter().position(|i| i.id == id) else {
                            return Ok(None);
                        };
                        let Some(to) = at.checked_add_signed(delta).filter(|t| *t < own.len())
                        else {
                            // A genuine edge, told apart from staleness by the anchor check above.
                            return Ok(Some(Vec::new()));
                        };
                        order = own.iter().map(|i| i.id.to_string()).collect();
                        order.swap(at, to);
                    }
                }

                let mut applied = Vec::with_capacity(order.len());
                for (position, sibling) in order.iter().enumerate() {
                    let position = position as u32;
                    if let Some(row) = instances.iter_mut().find(|i| i.id == *sibling) {
                        row.sort_index = Some(position);
                        if row.id == id {
                            if let Some(group) = &target_group {
                                row.group_path = group.clone();
                            }
                        }
                        applied.push((row.id.clone(), position, target_group.clone()));
                    }
                }
                Ok(Some(applied))
            })?;

        let Some(moved) = moved else {
            return Ok(MoveOutcome::Stale);
        };
        if moved.is_empty() {
            return Ok(MoveOutcome::AtEdge);
        }
        // Reconcile the view with what the store now holds.
        for (row_id, position, group) in &moved {
            if let Some(row) = self.instances.get_mut(row_id) {
                row.sort_index = Some(*position);
                if row_id == id {
                    if let Some(group) = group {
                        row.group_path = group.clone();
                    }
                }
            }
        }
        Ok(MoveOutcome::Moved)
    }

    /// Group paths in the order the list shows them, restricted to `profile`, including the
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

    /// Move a session out of its group into the neighbour in `delta`'s direction, landing
    /// against the boundary it crossed: at the bottom of the group above, the top of the
    /// group below, so a held key walks the row through the list. The neighbour is looked up
    /// inside the row's own profile, so a move never lands it in another profile's group.
    pub(super) fn move_session_across_groups(
        &mut self,
        id: &str,
        delta: isize,
    ) -> anyhow::Result<()> {
        let Some((current, profile)) = self
            .instances
            .get(id)
            .map(|i| (i.group_path.clone(), i.source_profile.clone()))
        else {
            return Ok(());
        };
        let groups = self.displayed_group_order(Some(&profile));
        let Some(at) = groups.iter().position(|g| *g == current) else {
            return Ok(());
        };
        let Some(target) = at
            .checked_add_signed(delta)
            .and_then(|idx| groups.get(idx))
            .cloned()
        else {
            return Ok(());
        };

        match self.apply_move(id, delta, Some(target.clone()))? {
            MoveOutcome::Moved => {}
            MoveOutcome::AtEdge => return Ok(()),
            MoveOutcome::Stale => return self.refresh_after_stale_move(),
        }
        // A collapsed destination would hide the row it just received: the cursor could not
        // follow it, and the next move would act on a session that is no longer on screen.
        let revealed = self.reveal_group(&profile, &target);
        self.after_committed_cross_group_move(id, &target, revealed);
        Ok(())
    }

    /// Settle the list after a move that is already stored. The row has changed group on disk
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

    /// Expand `path` and its ancestors so a row moved into them stays visible.
    fn reveal_group(&mut self, profile: &str, path: &str) -> anyhow::Result<()> {
        let mut wanted: Vec<String> = Vec::new();
        let mut walk = path;
        loop {
            wanted.push(walk.to_string());
            match walk.rsplit_once('/') {
                Some((parent, _)) if !parent.is_empty() => walk = parent,
                _ => break,
            }
        }
        let collapsed_now: Vec<String> = self
            .group_trees
            .get(profile)
            .map(|tree| {
                tree.get_all_groups()
                    .into_iter()
                    .filter(|g| g.collapsed && wanted.contains(&g.path))
                    .map(|g| g.path)
                    .collect()
            })
            .unwrap_or_default();
        if collapsed_now.is_empty() {
            return Ok(());
        }
        if let Some(storage) = self.storages.get(profile) {
            storage.update(|_instances, groups| {
                for group in groups.iter_mut() {
                    if collapsed_now.contains(&group.path) {
                        group.collapsed = false;
                    }
                }
                Ok(())
            })?;
        }
        if let Some(tree) = self.group_trees.get_mut(profile) {
            for path in &collapsed_now {
                tree.set_collapsed(path, false);
            }
        }
        Ok(())
    }

    fn move_group_row(&mut self, group_path: &str, delta: isize) -> anyhow::Result<()> {
        // The header under the cursor carries its own profile; `active_profile` is only the
        // fallback, and picking any storage key would reorder a different profile's groups.
        let Some(profile) = self
            .selected_group_profile
            .clone()
            .or_else(|| self.active_profile.clone())
            .or_else(|| self.sole_storage_profile())
        else {
            return Ok(());
        };
        let Some(storage) = self.storages.get(&profile) else {
            return Ok(());
        };
        // Collapsed state is the view's, not the store's: rebuilding the tree from disk
        // inside the transaction would otherwise re-expand what the user folded.
        let collapsed: HashMap<String, bool> = self
            .group_trees
            .get(&profile)
            .map(|t| {
                t.get_all_groups()
                    .into_iter()
                    .map(|g| (g.path, g.collapsed))
                    .collect()
            })
            .unwrap_or_default();

        // Build the tree from what the store actually holds, inside the lock. Moving the
        // in-memory tree and writing that back would resurrect a group a peer deleted while
        // this view was open, and drop metadata the peer changed.
        let moved: GroupMoveOutcome = storage.update(|instances, disk_groups| {
            let mut tree = GroupTree::new_with_groups(instances, disk_groups);
            if !tree.get_all_groups().iter().any(|g| g.path == group_path) {
                // The header under the cursor was drawn from an older read; a peer has since
                // deleted the group or emptied it.
                return Ok(GroupMoveOutcome::Stale);
            }
            if !tree.move_group(group_path, delta) {
                return Ok(GroupMoveOutcome::AtEdge);
            }
            let mut groups = tree.get_all_groups();
            for g in &mut groups {
                if let Some(state) = collapsed.get(&g.path) {
                    g.collapsed = *state;
                }
            }
            *disk_groups = groups.clone();
            Ok(GroupMoveOutcome::Moved(groups))
        })?;

        let groups = match moved {
            GroupMoveOutcome::Moved(groups) => groups,
            GroupMoveOutcome::AtEdge => return Ok(()),
            GroupMoveOutcome::Stale => return self.refresh_after_stale_move(),
        };
        let instances = self.cloned_instances_for_profile(&profile);
        self.group_trees
            .insert(profile, GroupTree::new_with_groups(&instances, &groups));
        self.rebuild_flat_items_keeping_cursor();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::group_contains;

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

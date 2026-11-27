//! Moving rows by hand. A row moves within its header by rewriting the shared Custom
//! order (`PUT /api/workspace-ordering`), the order the web dashboard's drag writes too.
//! Under Groups a row at the edge of its group crosses into the next one. A move under
//! Name or Newest first switches to Custom, which can be undone once from the command
//! palette.

use super::*;
use crate::session::config::{GroupByMode, SortOrder};

/// The persisted shared order and sort from before a switch to Custom, which Undo writes
/// back. Workspaces no move placed stay unplaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct OrderUndo {
    order: Vec<String>,
    repositories: Vec<String>,
    sort: SortOrder,
    /// A freeze's profile and its stored group order before the freeze replaced it.
    groups: Option<(String, Vec<String>)>,
}

/// What Undo in the command palette does next.
#[derive(Debug)]
pub(super) enum UndoPoint {
    /// Write back `restore` while the Daemon of `epoch` holds the orders the switch left,
    /// `groups` being the stored group order of `restore`'s freeze.
    Order {
        restore: OrderUndo,
        order: Vec<String>,
        repositories: Vec<String>,
        groups: Vec<String>,
        epoch: String,
    },
    /// The order is back but the sort write failed, so only the sort is retried.
    Sort(SortOrder),
}

/// The sort an Undo is writing back. `behind` while it waits behind another sidebar view
/// write, whose outcome is not the sort's.
#[derive(Debug)]
pub(super) struct SortRestore {
    sort: SortOrder,
    behind: bool,
}

/// An order write whose outcome the list acts on once it commits.
#[derive(Debug)]
pub(in crate::tui) enum OrderChange {
    /// A move under Name or Newest first; the sort becomes Custom. `expected` holds the
    /// stored row and header orders the write leaves, so a receipt that already holds a
    /// peer's change keeps no Undo.
    SwitchToCustom {
        undo: OrderUndo,
        expected: (Vec<String>, Vec<String>),
        /// `sort_changes` when the move began.
        sort_changes: u64,
    },
    /// Undo writing back this order; its sort follows once it commits.
    Undo(OrderUndo),
}

impl HomeView {
    /// Move the cursor's row one slot in `delta` (-1 up, 1 down). Under Name or Newest
    /// first the list switches to Custom, starting from the order on screen.
    pub(super) fn move_row_at_cursor(&mut self, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        // Each move is built from the applied order, so wait until the last one shows.
        if self.refuse_while_a_move_saves() {
            return Ok(());
        }
        if let Some(path) = self.selected_group.clone() {
            return self.move_header_row(&path, delta);
        }
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        if self
            .get_instance(&id)
            .is_some_and(|inst| inst.is_archived() || inst.is_trashed())
        {
            self.flash_status("Archived and trashed rows keep their own order");
            return Ok(());
        }
        let (Some(at), Some(workspace)) = (
            self.session_row(&id),
            self.get_instance(&id).map(Instance::workspace_id),
        ) else {
            return Ok(());
        };
        match self.sibling_session(at, delta, &workspace) {
            (Some(neighbour), _) => self.place_workspace(at, &id, &workspace, &neighbour, delta),
            (None, true) => {
                self.flash_status("Sessions on the same branch move together");
                Ok(())
            }
            (None, false) if self.group_by == GroupByMode::Group => self.carry_session(&id, delta),
            (None, false) => Ok(()),
        }
    }

    /// Flash and return true while an order write or a carry or group move is unapplied,
    /// since a move built before it shows would undo it, or before any snapshot applies,
    /// since a move carries the stored order it replaces.
    fn refuse_while_a_move_saves(&mut self) -> bool {
        if self.session_feed.applied_snapshot().is_none() {
            self.flash_status("The list is still loading from the Daemon");
            return true;
        }
        let pending = self.session_feed.ordering_pending()
            || matches!(
                self.pending_namespace_intent,
                Some(NamespaceIntent::ReorderSession { .. } | NamespaceIntent::ReorderGroup { .. })
            );
        if pending {
            self.flash_status("The last move is still saving");
        }
        pending
    }

    /// The workspaces of the session rows sharing row `at`'s header, in the order shown.
    fn header_workspaces(&self, at: usize) -> Vec<String> {
        let Some(depth) = self.flat_items.get(at).map(Item::depth) else {
            return Vec::new();
        };
        let sibling = |item: &Item| matches!(item, Item::Session { depth: d, .. } if *d == depth);
        let start = self.flat_items[..at]
            .iter()
            .rposition(|item| !sibling(item))
            .map_or(0, |i| i + 1);
        self.flat_items[start..]
            .iter()
            .take_while(|item| sibling(item))
            .filter_map(|item| match item {
                Item::Session { id, .. } => self.get_instance(id).map(Instance::workspace_id),
                _ => None,
            })
            .collect()
    }

    /// The nearest session row from `at` under the same header on another workspace than
    /// `workspace`, and whether rows on `workspace` were passed to reach it. A header's own
    /// sessions come before its child headers, so a header at the row's depth ends the run.
    fn sibling_session(&self, at: usize, delta: isize, workspace: &str) -> (Option<String>, bool) {
        let Some(depth) = self.flat_items.get(at).map(Item::depth) else {
            return (None, false);
        };
        let mut passed = false;
        let mut cursor = at;
        while let Some(next) = cursor.checked_add_signed(delta) {
            cursor = next;
            match self.flat_items.get(cursor) {
                Some(Item::Session { id, depth: d }) if *d == depth => {
                    let other = self.get_instance(id).map(Instance::workspace_id);
                    if other.as_deref() != Some(workspace) {
                        return (Some(id.clone()), passed);
                    }
                    passed = true;
                }
                Some(Item::Session { depth: d, .. } | Item::Group { depth: d, .. })
                    if *d > depth => {}
                _ => break,
            }
        }
        (None, passed)
    }

    /// Put session `id`'s workspace `a`, on row `at`, just before `neighbour`'s moving up and just after
    /// it moving down, in the stored order, or in the order on screen under Name or Newest
    /// first, which then switches to Custom. A workspace whose rows the sort splits apart
    /// still lands on the side it moved to.
    fn place_workspace(
        &mut self,
        at: usize,
        id: &str,
        a: &str,
        neighbour: &str,
        delta: isize,
    ) -> anyhow::Result<()> {
        let Some(b) = self.get_instance(neighbour).map(Instance::workspace_id) else {
            return self.refresh_after_stale_move();
        };
        let a = a.to_owned();
        let freeze = self.sort_order != SortOrder::Custom;
        let mut order = if freeze {
            self.displayed_workspace_order()
        } else {
            // Only the moved row's header is placed; other unplaced rows stay newest first.
            let placed = self.ordering_base().order;
            let header = self.header_workspaces(at);
            self.shared_workspace_order()
                .into_iter()
                .filter(|w| placed.contains(w) || header.contains(w))
                .collect()
        };
        order.retain(|w| *w != a);
        if !order.contains(&b) {
            order.push(b.clone());
        }
        let at = order.iter().position(|w| *w == b).unwrap();
        order.insert(if delta < 0 { at } else { at + 1 }, a.clone());
        let repositories = (freeze && self.group_by == GroupByMode::Project)
            .then(|| keep_unshown(self.displayed_repository_order(), &self.repository_order));
        // The write drops what the Daemon refuses; without both rows the move cannot show.
        let sessions = self
            .session_feed
            .applied_snapshot()
            .map(|snapshot| snapshot.contents.sessions.clone())
            .unwrap_or_default();
        let order = crate::server::api::sessions::writable_workspace_order(&sessions, order);
        if !order.contains(&a) || !order.contains(&b) {
            if [&a, &b]
                .iter()
                .any(|id| id.len() > crate::server::api::sessions::MAX_ORDER_ENTRY_LEN)
            {
                self.flash_status("This row's workspace id is too long for the Custom order");
                return Ok(());
            }
            return self.refresh_after_stale_move();
        }
        if freeze {
            let repositories =
                repositories.map(crate::server::api::sessions::writable_repository_order);
            return self.freeze_in_place(id, order, repositories, delta);
        }
        self.submit_custom_order(Some(order), repositories)
    }

    /// Commit `order`, and `repositories` under Repository, with the Group header order on
    /// screen, leaving session `id` in its group, so the switch to Custom keeps every header
    /// where it is.
    fn freeze_in_place(
        &mut self,
        id: &str,
        order: Vec<String>,
        repositories: Option<Vec<String>>,
        delta: isize,
    ) -> anyhow::Result<()> {
        let Some(row) = self.instances.get(id) else {
            return self.refresh_after_stale_move();
        };
        let (profile, source_group) = (row.source_profile.clone(), row.group_path.clone());
        self.freeze_through(id, profile, source_group, order, repositories, delta)
    }

    /// `freeze_in_place` through session `id` of `profile` in `source_group`, which the
    /// move names but leaves where it is.
    fn freeze_through(
        &mut self,
        id: &str,
        profile: String,
        source_group: String,
        order: Vec<String>,
        repositories: Option<Vec<String>>,
        delta: isize,
    ) -> anyhow::Result<()> {
        let headers = if self.group_by == GroupByMode::Group {
            self.displayed_headers(&profile)
        } else {
            let Some(headers) = self.displayed_nested_headers(&profile) else {
                self.flash_status("Pick Custom first: the Groups can't keep this order");
                return Ok(());
            };
            headers
        };
        let (freeze, switch) = self.frozen_order(order, &profile, repositories);
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Session {
                id: id.to_owned(),
                profile,
                source_group,
                destination: None,
                direction: move_direction(delta),
                headers: Some(headers),
                freeze: Some(freeze),
            }),
            NamespaceIntent::ReorderSession {
                id: id.to_owned(),
                switch: Some(switch),
            },
        )
    }

    /// The profiles whose Groups the Repository headers show nested, by name.
    fn nested_header_profiles(&self) -> Vec<String> {
        let mut profiles: Vec<String> = self
            .group_trees
            .iter()
            .filter(|(profile, _)| {
                self.active_profile
                    .as_deref()
                    .is_none_or(|active| active == profile.as_str())
            })
            .filter(|(_, tree)| {
                self.flat_items.iter().any(|item| {
                    matches!(item, Item::Group { path, .. }
                        if super::rows::split_project_view_path(path)
                            .1
                            .is_some_and(|path| tree.group_exists(path)))
                })
            })
            .map(|(profile, _)| profile.clone())
            .collect();
        profiles.sort();
        profiles
    }

    /// `profile`'s Groups nested under Repository headers, in the order the list shows them,
    /// or None when two Repositories show them in orders no one profile-wide order keeps.
    fn displayed_nested_headers(&self, profile: &str) -> Option<Vec<String>> {
        let tree = self.group_trees.get(profile);
        let mut shown: Vec<(&str, Vec<&str>)> = Vec::new();
        for item in &self.flat_items {
            let Item::Group { path, .. } = item else {
                continue;
            };
            if let (repository, Some(path)) = super::rows::split_project_view_path(path) {
                if tree.is_some_and(|t| t.group_exists(path)) {
                    match shown.iter_mut().find(|(r, _)| *r == repository) {
                        Some((_, paths)) => paths.push(path),
                        None => shown.push((repository, vec![path])),
                    }
                }
            }
        }
        let mut headers: Vec<String> = Vec::new();
        for path in shown.iter().flat_map(|(_, paths)| paths) {
            if !headers.iter().any(|h| h == path) {
                headers.push((*path).to_owned());
            }
        }
        let rank = |path: &str| headers.iter().position(|h| h == path);
        shown
            .iter()
            .all(|(_, paths)| paths.windows(2).all(|w| rank(w[0]) < rank(w[1])))
            .then_some(headers)
    }

    /// `profile`'s group headers in the order the list shows them.
    fn displayed_headers(&self, profile: &str) -> Vec<String> {
        self.displayed_group_order(Some(profile))
            .into_iter()
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Write a Custom order. One taken from the screen switches the sort to Custom once it
    /// commits, so a refused write leaves the sort alone.
    fn submit_custom_order(
        &mut self,
        order: Option<Vec<String>>,
        repositories: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        let change = (self.sort_order != SortOrder::Custom).then(|| OrderChange::SwitchToCustom {
            undo: self.order_undo_point(),
            expected: Default::default(),
            sort_changes: self.sort_changes,
        });
        self.write_order(order, repositories, change)
    }

    /// Write back the order, and the sort, from before the last switch to Custom.
    pub(super) fn undo_order_change(&mut self) -> anyhow::Result<()> {
        if self.refuse_while_a_move_saves() {
            return Ok(());
        }
        match self.order_undo.take() {
            None => self.flash_status("No order change to undo"),
            Some(UndoPoint::Sort(_)) if self.sort_order != SortOrder::Custom => {
                self.flash_status("The sort changed elsewhere, nothing to undo");
            }
            Some(UndoPoint::Sort(sort)) => {
                self.restore_sort(sort);
            }
            Some(UndoPoint::Order {
                restore,
                order,
                repositories,
                groups,
                epoch,
            }) => {
                let base = self.ordering_base();
                let applied = self
                    .session_feed
                    .applied_snapshot()
                    .map(|snapshot| snapshot.cursor.epoch.clone());
                if applied.as_ref() != Some(&epoch) {
                    self.flash_status("The Daemon restarted, nothing to undo");
                    return Ok(());
                }
                if base.order != order
                    || base.repositories != repositories
                    || restore
                        .groups
                        .as_ref()
                        .is_some_and(|(profile, _)| self.stored_groups(profile) != groups)
                {
                    self.flash_status("The order changed elsewhere, nothing to undo");
                    return Ok(());
                }
                let change = OrderChange::Undo(restore.clone());
                let written = (restore.order.clone(), restore.repositories.clone());
                let frozen = restore.groups.clone();
                // Kept until the write commits, so a refused one can be retried.
                self.order_undo = Some(UndoPoint::Order {
                    restore,
                    order,
                    repositories,
                    groups: groups.clone(),
                    epoch,
                });
                if let Some((profile, before)) = frozen {
                    match self.group_order_anchor(&profile) {
                        Some(anchor) => {
                            return self.undo_frozen_groups(
                                anchor, profile, before, written, groups, change,
                            );
                        }
                        // Only a move puts a Group order back, and it needs a row to name.
                        None if before != groups => {
                            self.flash_status(
                                "Undo needs a listed session in that profile to put its Groups back",
                            );
                            return Ok(());
                        }
                        None => {}
                    }
                }
                return self.write_order(Some(written.0), Some(written.1), Some(change));
            }
        }
        Ok(())
    }

    /// Write back a freeze's row and group orders in one move, refused as stale once either
    /// changed since the switch. The move needs a row of the profile to anchor it.
    fn undo_frozen_groups(
        &mut self,
        (id, source_group): (String, String),
        profile: String,
        before: Vec<String>,
        (order, repositories): (Vec<String>, Vec<String>),
        after: Vec<String>,
        change: OrderChange,
    ) -> anyhow::Result<()> {
        let freeze = crate::daemon::FrozenOrder {
            order,
            base: self.ordering_base(),
            groups: after,
            repositories: Some(repositories),
        };
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Session {
                id,
                profile,
                source_group,
                destination: None,
                direction: crate::daemon::MoveDirection::Down,
                headers: Some(before),
                freeze: Some(freeze),
            }),
            NamespaceIntent::ReorderGroup {
                switch: Some(change),
            },
        )
    }

    /// A listed row of `profile` and its group, which a move restoring its group order names.
    fn group_order_anchor(&self, profile: &str) -> Option<(String, String)> {
        self.instances
            .values()
            .filter(|row| row.source_profile == profile && !row.is_archived() && !row.is_trashed())
            .map(|row| (row.id.clone(), row.group_path.clone()))
            .min()
    }

    /// `profile`'s stored group paths, in stored order, as the Daemon last published them.
    fn stored_groups(&self, profile: &str) -> Vec<String> {
        self.session_feed
            .applied_snapshot()
            .and_then(|snapshot| {
                snapshot
                    .contents
                    .profiles
                    .iter()
                    .find(|p| p.name == profile)
                    .map(|p| p.groups.iter().map(|group| group.path.clone()).collect())
            })
            .unwrap_or_default()
    }

    /// Pick `sort` again for Undo, retried from the palette when the write fails. Returns
    /// whether the write was sent or queued.
    fn restore_sort(&mut self, sort: SortOrder) -> bool {
        let behind = self.session_feed.sidebar_view_pending();
        self.show_sidebar_view(self.group_by, sort);
        let patch = crate::daemon::SidebarViewPatch {
            sort: Some(sort),
            ..Default::default()
        };
        if let Err(error) = self.save_sidebar_view(patch) {
            self.order_undo = Some(UndoPoint::Sort(sort));
            self.flash_status(format!(
                "Order restored, sort not saved: {error} (Undo in Ctrl+K retries it)"
            ));
            return false;
        }
        self.sort_restore = Some(SortRestore { sort, behind });
        true
    }

    /// Act on a settled sidebar view write. A failed one carrying Undo's sort leaves the
    /// sort to retry.
    pub(super) fn sidebar_view_settled(
        &mut self,
        result: &Result<crate::tui::session_feed::ViewWriteSettled, String>,
    ) {
        // A write the sort waited behind sends the sort as it settles.
        let sent_since = self.session_feed.sidebar_view_pending();
        let ours = match &mut self.sort_restore {
            Some(restore) if restore.behind && sent_since => {
                restore.behind = false;
                false
            }
            restore => restore.is_some(),
        };
        let sort = if ours {
            self.sort_restore.take().map(|restore| restore.sort)
        } else {
            None
        };
        match (result, sort) {
            (Err(error), Some(sort)) => {
                self.order_undo = Some(UndoPoint::Sort(sort));
                self.flash_status(format!(
                    "Order restored, sort not saved: {error} (Undo in Ctrl+K retries it)"
                ));
            }
            (Err(error), None) => {
                self.flash_status(format!("Grouping and sort not saved: {error}"));
            }
            // A replacement Daemon's frame came first; one still on another sort leaves the
            // sort to retry.
            (Ok(crate::tui::session_feed::ViewWriteSettled::Superseded), Some(sort))
                if self
                    .session_feed
                    .applied_snapshot()
                    .is_some_and(|snapshot| snapshot.contents.sidebar_view.sort != sort) =>
            {
                self.order_undo = Some(UndoPoint::Sort(sort));
                self.flash_status(
                    "Order restored, sort unconfirmed after a Daemon restart (Undo in Ctrl+K retries it)",
                );
            }
            (Ok(_), _) => {}
        }
    }

    /// Send an order write. Any order change but Undo's own makes the remembered Undo
    /// stale; Undo keeps it until the write commits, so a refused one can be retried.
    fn write_order(
        &mut self,
        order: Option<Vec<String>>,
        repositories: Option<Vec<String>>,
        mut change: Option<OrderChange>,
    ) -> anyhow::Result<()> {
        use crate::server::api::sessions::{writable_repository_order, writable_workspace_order};
        let snapshot = self.session_feed.applied_snapshot();
        let sessions = snapshot
            .as_ref()
            .map(|snapshot| snapshot.contents.sessions.as_slice())
            .unwrap_or_default();
        // The Daemon refuses the write once a peer has changed the order it replaces.
        let base = self.ordering_base();
        let order = order.map(|order| writable_workspace_order(sessions, order));
        let repositories = repositories.map(writable_repository_order);
        if let Some(OrderChange::SwitchToCustom { expected, .. }) = &mut change {
            *expected = (
                order.clone().unwrap_or_else(|| base.order.clone()),
                repositories
                    .clone()
                    .unwrap_or_else(|| base.repositories.clone()),
            );
        }
        self.session_feed
            .submit_workspace_ordering(crate::daemon::WorkspaceOrderingPatch {
                order,
                repositories,
                base: Some(base),
                ..Default::default()
            })?;
        if !matches!(change, Some(OrderChange::Undo(_))) {
            self.order_undo = None;
        }
        self.pending_order_change = change;
        Ok(())
    }

    /// The order a move under Name or Newest first commits with the move, and the switch to
    /// Custom that follows it. `profile`'s stored group order goes with it, since the move
    /// replaces that too, and `repositories`, the Repository header order it commits.
    fn frozen_order(
        &self,
        order: Vec<String>,
        profile: &str,
        repositories: Option<Vec<String>>,
    ) -> (crate::daemon::FrozenOrder, OrderChange) {
        let snapshot = self.session_feed.applied_snapshot();
        let sessions = snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.contents.sessions.as_slice());
        let order = crate::server::api::sessions::writable_workspace_order(sessions, order);
        let groups = self.stored_groups(profile);
        let base = self.ordering_base();
        let change = OrderChange::SwitchToCustom {
            undo: OrderUndo {
                groups: Some((profile.to_owned(), groups.clone())),
                ..self.order_undo_point()
            },
            expected: (
                order.clone(),
                repositories
                    .clone()
                    .unwrap_or_else(|| base.repositories.clone()),
            ),
            sort_changes: self.sort_changes,
        };
        (
            crate::daemon::FrozenOrder {
                order,
                base,
                groups,
                repositories,
            },
            change,
        )
    }

    /// The stored row and header orders as the Daemon last published them.
    fn ordering_base(&self) -> crate::daemon::WorkspaceOrderingBase {
        let snapshot = self.session_feed.applied_snapshot();
        let contents = snapshot.as_ref().map(|snapshot| &snapshot.contents);
        crate::daemon::WorkspaceOrderingBase {
            order: contents
                .map(|c| c.placed_workspaces.clone())
                .unwrap_or_default(),
            repositories: contents
                .map(|c| c.repository_order.clone())
                .unwrap_or_default(),
        }
    }

    /// The persisted order and sort Undo restores, as the Daemon last published them.
    fn order_undo_point(&self) -> OrderUndo {
        let base = self.ordering_base();
        OrderUndo {
            order: base.order,
            repositories: base.repositories,
            sort: self.sort_order,
            groups: None,
        }
    }

    /// Act on an order change whose `receipt` is applied. The Undo point is the stored order
    /// at that revision when it is the one this change left; a later revision, or one that
    /// already holds a peer's change, keeps none.
    pub(super) fn order_change_committed(
        &mut self,
        change: OrderChange,
        receipt: &crate::daemon::RuntimeCursor,
    ) {
        match change {
            OrderChange::SwitchToCustom {
                undo,
                expected,
                sort_changes,
            } => {
                let base = self.ordering_base();
                let kept = self
                    .session_feed
                    .applied_snapshot()
                    .is_some_and(|snapshot| snapshot.cursor == *receipt)
                    && (&base.order, &base.repositories) == (&expected.0, &expected.1);
                // A sort picked since the move started wins, even one back on the same value.
                let picked = self.sort_changes != sort_changes || self.sort_order != undo.sort;
                if picked && self.sort_order != SortOrder::Custom {
                    self.order_undo = None;
                    self.flash_status("Order saved, the sort picked since stays");
                    return;
                }
                if self.sort_order != SortOrder::Custom {
                    self.apply_sort_order(SortOrder::Custom);
                }
                let groups = undo
                    .groups
                    .as_ref()
                    .map(|(profile, _)| self.stored_groups(profile))
                    .unwrap_or_default();
                self.order_undo = kept.then_some(UndoPoint::Order {
                    restore: undo,
                    order: expected.0,
                    repositories: expected.1,
                    groups,
                    epoch: receipt.epoch.clone(),
                });
                self.flash_status(if kept {
                    "Switched to Custom (Undo in Ctrl+K)"
                } else {
                    "Switched to Custom"
                });
            }
            OrderChange::Undo(undo) => {
                self.order_undo = None;
                // A sort picked since the switch wins.
                if self.sort_order != SortOrder::Custom
                    || undo.sort == SortOrder::Custom
                    || self.restore_sort(undo.sort)
                {
                    self.flash_status("Order change undone");
                }
            }
        }
    }

    /// The Daemon's merged order: every workspace, unmoved ones first.
    fn shared_workspace_order(&self) -> Vec<String> {
        self.session_feed
            .applied_snapshot()
            .map(|snapshot| snapshot.contents.workspace_ordering.clone())
            .unwrap_or_default()
    }

    /// Workspaces in the order the list shows them, then any it hides, in shared order.
    fn displayed_workspace_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for item in &self.flat_items {
            if let Item::Session { id, .. } = item {
                if let Some(workspace) = self.get_instance(id).map(Instance::workspace_id) {
                    if !order.contains(&workspace) {
                        order.push(workspace);
                    }
                }
            }
        }
        for workspace in self.shared_workspace_order() {
            if !order.contains(&workspace) {
                order.push(workspace);
            }
        }
        order
    }

    /// Repository paths in the order the Repository headers show them.
    fn displayed_repository_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for item in &self.flat_items {
            if let Item::Group { path, depth: 0, .. } = item {
                if let Some(repo) = self.repository_path_for_header(path) {
                    if !order.contains(&repo) {
                        order.push(repo);
                    }
                }
            }
        }
        order
    }

    /// The repository path behind a top-level Repository header, keyed by its label. Two
    /// repositories can share a label; the one whose live session places the header, as
    /// `build_flat_items_by_project` ranks them in the current view and sort, is the one a
    /// move rewrites.
    fn repository_path_for_header(&self, label: &str) -> Option<String> {
        if crate::session::is_synthetic_project_header(label) || label.contains('/') {
            return None;
        }
        let rank = |repo: &str| {
            (self.sort_order == SortOrder::Custom)
                .then(|| {
                    self.repository_order
                        .iter()
                        .position(|r| r.trim_end_matches('/') == repo)
                })
                .flatten()
        };
        let profile = self.active_profile.as_deref();
        // An empty header is a pinned project's, chosen as the renderer chooses it.
        self.instances
            .values()
            .filter(|inst| {
                profile.is_none_or(|p| inst.source_profile == p)
                    && !inst.scratch
                    && !inst.is_archived()
                    && !inst.is_trashed()
                    && project_group_key(inst) == label
            })
            .map(|inst| {
                let repo = inst.repo_path().trim_end_matches('/').to_string();
                let rank = rank(&repo);
                (
                    (rank.is_some(), rank, std::cmp::Reverse(inst.created_at)),
                    repo,
                )
            })
            .min()
            .map(|(_, repo)| repo)
            .or_else(|| {
                crate::session::projects::unpopulated_projects(
                    &std::collections::HashSet::new(),
                    &self.registered_projects,
                )
                .into_iter()
                .filter(|p| p.label == label)
                .min_by_key(|p| {
                    let rank = rank(p.path.trim_end_matches('/'));
                    (rank.is_some(), rank)
                })
                .map(|p| p.path.trim_end_matches('/').to_string())
            })
    }

    fn move_header_row(&mut self, path: &str, delta: isize) -> anyhow::Result<()> {
        let nested = super::rows::split_project_view_path(path).1.is_some();
        if self.group_by == GroupByMode::Group || nested {
            // Under Repository a nested header moves only under Custom; Groups switches from Name.
            if nested && self.group_by != GroupByMode::Group && self.sort_order != SortOrder::Custom
            {
                self.flash_status("Pick the Custom sort (o) to arrange groups by hand");
                return Ok(());
            }
            return self.move_group_row(path, delta);
        }
        let Some(repo) = self.repository_path_for_header(path) else {
            self.flash_status("This header does not move");
            return Ok(());
        };
        let mut order = self.displayed_repository_order();
        let Some(at) = order.iter().position(|r| *r == repo) else {
            return Ok(());
        };
        let Some(to) = at.checked_add_signed(delta).filter(|to| *to < order.len()) else {
            return Ok(());
        };
        let other = order[to].clone();
        order.swap(at, to);
        // Under Custom only the two headers gain a place; unplaced ones stay newest first.
        if self.sort_order == SortOrder::Custom {
            let placed: Vec<&str> = self
                .repository_order
                .iter()
                .map(|r| r.trim_end_matches('/'))
                .collect();
            order.retain(|r| placed.contains(&r.as_str()) || *r == repo || *r == other);
        }
        let rows = (self.sort_order != SortOrder::Custom).then(|| self.displayed_workspace_order());
        let order = crate::server::api::sessions::writable_repository_order(keep_unshown(
            order,
            &self.repository_order,
        ));
        // The write drops what the Daemon refuses; without both headers the move cannot show.
        if !order.contains(&repo) || !order.contains(&other) {
            self.flash_status("This repository's path is too long for the Custom order");
            return Ok(());
        }
        // From Name or Newest first the nested Groups freeze with the rows, as a row move's do.
        if let Some(rows) = rows {
            match self.nested_header_profiles().as_slice() {
                [] => {}
                [profile] => {
                    let Some((id, source_group)) = self.group_order_anchor(profile) else {
                        return self.refresh_after_stale_move();
                    };
                    let profile = profile.clone();
                    return self.freeze_through(
                        &id,
                        profile,
                        source_group,
                        rows,
                        Some(order),
                        delta,
                    );
                }
                _ => {
                    self.flash_status("Pick Custom first: the Groups can't keep this order");
                    return Ok(());
                }
            }
            return self.submit_custom_order(Some(rows), Some(order));
        }
        self.submit_custom_order(rows, Some(order))
    }

    /// Carry a session across its group's edge into the neighbouring group on screen.
    fn carry_session(&mut self, id: &str, delta: isize) -> anyhow::Result<()> {
        let row = self
            .instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("The selected row changed elsewhere"))?;
        let profile = row.source_profile.clone();
        let source_group = row.group_path.clone();
        let groups = self.displayed_group_order(Some(&profile));
        let Some(destination) = groups
            .iter()
            .position(|group| *group == source_group)
            .and_then(|at| at.checked_add_signed(delta))
            .and_then(|at| groups.get(at))
            .map(|path| (*path).to_owned())
        else {
            return Ok(());
        };
        let moved = row.workspace_id();
        let direction = move_direction(delta);
        // Under Name the carried row lands where a Custom carry puts it: at the edge of the
        // destination it enters from, in the order on screen.
        // Under Name the stored header order takes the one on screen too.
        let headers =
            (self.sort_order != SortOrder::Custom).then(|| self.displayed_headers(&profile));
        let frozen = (self.sort_order != SortOrder::Custom).then(|| {
            let peers: Vec<String> = self
                .instances
                .values()
                .filter(|row| {
                    row.source_profile == profile
                        && row.group_path == destination
                        && !row.is_archived()
                        && !row.is_trashed()
                })
                .map(Instance::workspace_id)
                .filter(|workspace| *workspace != moved)
                .collect();
            let order = self.displayed_workspace_order();
            let order = crate::server::api::place_at_entering_edge(
                order.clone(),
                &moved,
                &peers,
                direction,
            )
            .unwrap_or(order);
            self.frozen_order(order, &profile, None)
        });
        let (freeze, switch) = frozen.unzip();
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Session {
                id: id.to_owned(),
                profile,
                source_group,
                destination: Some(destination),
                direction,
                headers,
                freeze,
            }),
            NamespaceIntent::ReorderSession {
                id: id.to_owned(),
                switch,
            },
        )
    }

    /// Drop a move aimed at rows that have since changed and show the current list instead.
    /// Repeating the keystroke then acts on what is on screen.
    pub(super) fn refresh_after_stale_move(&mut self) -> anyhow::Result<()> {
        self.rebuild_flat_items_keeping_cursor();
        self.update_selected();
        self.flash_status("Rows changed elsewhere, list refreshed");
        Ok(())
    }

    /// Group paths in the order the list shows them, restricted to `profile`, including the
    /// ungrouped bucket that `flatten_tree` puts first. The synthetic Archived and Trash
    /// sections are left out: they are sinks a row reaches by being archived or trashed,
    /// never by being moved.
    fn displayed_group_order(&self, profile: Option<&str>) -> Vec<&str> {
        let mut order: Vec<&str> = Vec::new();
        for item in &self.flat_items {
            let path = match item {
                Item::Group {
                    path,
                    profile: row_profile,
                    ..
                } => {
                    // Single-profile headers omit their shared profile.
                    match (profile, row_profile.as_deref()) {
                        (Some(want), Some(have)) if want != have => continue,
                        _ => path.as_str(),
                    }
                }
                Item::Session { id, .. } => match self.get_instance(id) {
                    Some(inst)
                        if !inst.is_archived()
                            && !inst.is_trashed()
                            && profile.is_none_or(|p| inst.source_profile == p) =>
                    {
                        inst.group_path.as_str()
                    }
                    _ => continue,
                },
                Item::Suggested { .. } => continue,
            };
            if crate::session::is_within_archived_section(path)
                || crate::session::is_within_trash_section(path)
            {
                continue;
            }
            if !order.contains(&path) {
                order.push(path);
            }
        }
        order
    }

    /// Follow only the acknowledged original row, using transactionally revealed destination.
    pub(super) fn follow_committed_reorder(
        &mut self,
        id: &str,
        destination: Option<&crate::daemon::GroupLocation>,
    ) {
        self.rebuild_flat_items_keeping_cursor();
        if let Some(at) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id: row, .. } if row == id))
        {
            self.cursor = at;
            self.update_selected();
            return;
        }
        if let Some(destination) = destination {
            if let Some(at) = self.flat_items.iter().position(|item| matches!(item, Item::Group { path, profile, .. }
                if path == &destination.path && profile.as_deref().is_none_or(|name| name == destination.profile))) {
                self.cursor = at;
                self.update_selected();
            }
        }
    }

    /// Move a group among its siblings. Under Name the move starts from the order on
    /// screen, which the stored group order and the shared row order then keep.
    fn move_group_row(&mut self, header_path: &str, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        let group = if self.group_by == GroupByMode::Group {
            self.canonical_group_location(header_path)?
        } else {
            let Some(group) = self.nested_group_location(header_path) else {
                return Ok(());
            };
            group
        };
        let freeze = self.sort_order != SortOrder::Custom;
        // A stored Group with no shown session has no nested header, so only shown ones count.
        let siblings = if self.group_by == GroupByMode::Group {
            freeze.then(|| self.displayed_sibling_groups(&group))
        } else {
            Some(self.displayed_nested_siblings(header_path, &group))
        };
        let (freeze, switch) = freeze
            .then(|| self.frozen_order(self.displayed_workspace_order(), &group.profile, None))
            .unzip();
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Group {
                group,
                direction: move_direction(delta),
                siblings,
                freeze,
                base: None,
                base_sort: None,
            }),
            NamespaceIntent::ReorderGroup { switch },
        )
    }

    /// The profile Group behind a `repo/group` header under Repository: the one profile whose
    /// sessions sit under it. Flashes why when there is none.
    fn nested_group_location(&mut self, header_path: &str) -> Option<crate::daemon::GroupLocation> {
        let (_, Some(path)) = super::rows::split_project_view_path(header_path) else {
            return None;
        };
        let prefix = format!("{header_path}/");
        let mut owners: Vec<String> = self
            .instances
            .values()
            .filter(|inst| !inst.is_archived() && !inst.is_trashed())
            .filter(|inst| {
                self.active_profile
                    .as_ref()
                    .is_none_or(|p| inst.source_profile == *p)
            })
            .filter(|inst| {
                let at = project_view_path(inst);
                at == header_path || at.starts_with(&prefix)
            })
            .map(|inst| inst.source_profile.clone())
            .collect();
        owners.sort_unstable();
        owners.dedup();
        let location = match &owners[..] {
            [profile]
                if self
                    .group_trees
                    .get(profile)
                    .is_some_and(|t| t.group_exists(path)) =>
            {
                crate::daemon::GroupLocation {
                    profile: profile.clone(),
                    path: path.to_owned(),
                }
            }
            [_] | [] => {
                self.flash_status("The group changed elsewhere; no change submitted");
                return None;
            }
            _ => {
                self.flash_status("Groups move only within one profile");
                return None;
            }
        };
        Some(location)
    }

    /// The stored paths of the nested headers in `header_path`'s repository that share `group`'s
    /// parent, in the order the list shows them.
    fn displayed_nested_siblings(
        &self,
        header_path: &str,
        group: &crate::daemon::GroupLocation,
    ) -> Vec<String> {
        let parent = |path: &str| path.rsplit_once('/').map(|(head, _)| head.to_owned());
        let repo = super::rows::split_project_view_path(header_path).0;
        let mut siblings: Vec<String> = Vec::new();
        for item in &self.flat_items {
            let Item::Group { path, .. } = item else {
                continue;
            };
            let (label, Some(path)) = super::rows::split_project_view_path(path) else {
                continue;
            };
            if label == repo
                && parent(path) == parent(&group.path)
                && !siblings.iter().any(|s| s == path)
            {
                siblings.push(path.to_owned());
            }
        }
        siblings
    }

    /// The headers sharing `group`'s parent and profile, in the order the list shows them.
    fn displayed_sibling_groups(&self, group: &crate::daemon::GroupLocation) -> Vec<String> {
        let parent = |path: &str| path.rsplit_once('/').map(|(head, _)| head.to_owned());
        let mut siblings: Vec<String> = Vec::new();
        for item in &self.flat_items {
            if let Item::Group { path, profile, .. } = item {
                if parent(path) == parent(&group.path)
                    && profile.as_deref().is_none_or(|p| p == group.profile)
                    && !siblings.contains(path)
                {
                    siblings.push(path.clone());
                }
            }
        }
        siblings
    }
}

fn move_direction(delta: isize) -> crate::daemon::MoveDirection {
    if delta < 0 {
        crate::daemon::MoveDirection::Up
    } else {
        crate::daemon::MoveDirection::Down
    }
}

/// `shown` in its own order, with each `persisted` entry this view does not show (another
/// profile's repository, say) kept after the entry it followed, so other Clients keep its place.
/// Persisted entries compare without a trailing slash, as `shown` names them.
fn keep_unshown(mut shown: Vec<String>, persisted: &[String]) -> Vec<String> {
    let persisted: Vec<String> = persisted
        .iter()
        .map(|entry| entry.trim_end_matches('/').to_owned())
        .collect();
    let mut previous: Option<&String> = None;
    for entry in &persisted {
        if !shown.contains(entry) {
            let at = match previous {
                Some(previous) => shown.iter().position(|e| e == previous).map(|at| at + 1),
                None => shown.iter().position(|e| persisted.contains(e)),
            }
            .unwrap_or(shown.len());
            shown.insert(at, entry.clone());
        }
        previous = Some(entry);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::home::tests::{
        apply_published, native_state, payload_bytes, published_snapshot, receipt_cursor,
        request_with_headers,
    };

    #[test]
    fn keep_unshown_holds_hidden_entries_in_place() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (shown, persisted, expected) in [
            (&["b", "a"][..], &["x", "a", "b"][..], &["x", "b", "a"][..]),
            (&["b", "a"], &["a", "x", "b"], &["b", "a", "x"]),
            (
                &["new", "b", "a"],
                &["a", "b", "x"],
                &["new", "b", "x", "a"],
            ),
            (&["a", "new"], &["x", "a"], &["x", "a", "new"]),
            (&["a"], &[], &["a"]),
            (&["b", "a"], &["a/", "x/", "b"], &["b", "a", "x"]),
        ] {
            assert_eq!(
                keep_unshown(list(shown), &list(persisted)),
                list(expected),
                "{shown:?} over {persisted:?}"
            );
        }
    }

    /// The Undo point keeps the stored orders the switch left at its receipt: a later frame,
    /// or a receipt already holding a peer's change, keeps none; a peer's change to the
    /// stored orders makes it stale while a display-only one does not. Undo restores the
    /// sort only while it is still Custom, and a failed sort write leaves the sort to retry.
    #[test]
    #[serial_test::serial]
    fn the_undo_point_follows_the_stored_orders_at_its_receipt() {
        use crate::daemon::RuntimeCursor;
        use crate::tui::session_feed::{SessionFeedResult, ViewWrite};
        let _home = crate::session::test_support::isolate_app_dir();
        let mut view = HomeView::new_for_test(
            None,
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let mut capture = view.session_feed.view_write_driver_for_test();
        let publish = |view: &mut HomeView, epoch: &str, revision: u64, repositories: &[&str]| {
            let mut snapshot =
                crate::tui::home::tests::fixture_snapshot(Vec::new(), "default", epoch, revision);
            snapshot.contents.repository_order =
                repositories.iter().map(|r| r.to_string()).collect();
            view.session_feed
                .publish_for_test(SessionFeedResult::Snapshot(std::sync::Arc::new(snapshot)));
            view.apply_session_feed();
            RuntimeCursor {
                epoch: epoch.into(),
                revision,
            }
        };
        let flash = |view: &HomeView| view.status_flash.as_ref().map(|f| f.text.clone());
        let repositories = vec!["/r/b".to_string(), "/r/a".to_string()];
        let switch = |expected: Vec<String>, sort_changes: u64| OrderChange::SwitchToCustom {
            undo: OrderUndo {
                order: Vec::new(),
                repositories: Vec::new(),
                sort: SortOrder::Name,
                groups: None,
            },
            expected: (Vec::new(), expected),
            sort_changes,
        };
        let at = publish(&mut view, "e", 2, &["/r/b", "/r/a"]);
        // A Group order to put back needs a listed row to anchor its move.
        view.order_undo = Some(UndoPoint::Order {
            restore: OrderUndo {
                order: Vec::new(),
                repositories: Vec::new(),
                sort: SortOrder::Name,
                groups: Some(("default".into(), vec!["gone".into()])),
            },
            order: Vec::new(),
            repositories: repositories.clone(),
            groups: view.stored_groups("default"),
            epoch: "e".into(),
        });
        view.undo_order_change().unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("Undo needs a listed session in that profile to put its Groups back")
        );
        assert!(capture().is_none() && view.order_undo.is_some());
        view.order_undo = None;
        let earlier = RuntimeCursor {
            revision: 1,
            ..at.clone()
        };
        for (receipt, expected) in [(&earlier, repositories.clone()), (&at, Vec::new())] {
            view.order_change_committed(switch(expected, view.sort_changes), receipt);
            assert!(view.order_undo.is_none(), "{receipt:?}");
            assert_eq!(flash(&view).as_deref(), Some("Switched to Custom"));
        }
        view.order_change_committed(switch(repositories.clone(), view.sort_changes), &at);
        assert_eq!(
            flash(&view).as_deref(),
            Some("Switched to Custom (Undo in Ctrl+K)")
        );
        publish(&mut view, "e", 3, &[]);
        view.undo_order_change().unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("The order changed elsewhere, nothing to undo")
        );
        assert!(capture().is_none());

        // A revision that changed nothing stored still offers Undo, which a replacement
        // Daemon's frame then overtakes.
        publish(&mut view, "e", 4, &["/r/b", "/r/a"]);
        view.order_change_committed(
            switch(repositories.clone(), view.sort_changes),
            &RuntimeCursor {
                epoch: "e".into(),
                revision: 4,
            },
        );
        publish(&mut view, "e", 5, &["/r/b", "/r/a"]);
        view.undo_order_change().unwrap();
        capture()
            .expect("the restoring order")
            .finish(Ok(RuntimeCursor {
                epoch: "e".into(),
                revision: 9,
            }));
        publish(&mut view, "new", 1, &["/r/b", "/r/a"]);
        assert!(view.pending_order_change.is_none());
        assert_eq!(
            flash(&view).as_deref(),
            Some("The Daemon restarted before the order change showed")
        );
        // Undo stays with the Daemon that confirmed it.
        view.undo_order_change().unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("The Daemon restarted, nothing to undo")
        );
        assert!(capture().is_none());

        // A write whose reply is lost is not reported as refused.
        let restarted = RuntimeCursor {
            epoch: "new".into(),
            revision: 1,
        };
        view.order_change_committed(switch(repositories.clone(), view.sort_changes), &restarted);
        view.undo_order_change().unwrap();
        capture()
            .expect("the restoring order")
            .finish_unconfirmed("timed out");
        view.apply_session_feed();
        assert_eq!(
            flash(&view).as_deref(),
            Some("Order change unconfirmed (timed out); check the list before moving again")
        );

        // Undo leaves a sort picked since alone, and otherwise restores the one it switched
        // from, keeping it on offer while that write fails.
        let undo = |sort| {
            OrderChange::Undo(OrderUndo {
                order: Vec::new(),
                repositories: Vec::new(),
                sort,
                groups: None,
            })
        };
        view.sort_order = SortOrder::Newest;
        view.order_change_committed(undo(SortOrder::Name), &at);
        assert!(capture().is_none(), "a later pick wins");
        view.sort_order = SortOrder::Custom;
        view.order_change_committed(undo(SortOrder::Name), &at);
        let sort = capture().expect("the restored sort");
        assert!(
            matches!(&sort.write, ViewWrite::SidebarView(patch) if patch.sort == Some(SortOrder::Name))
        );
        sort.finish(Err("refused".into()));
        view.apply_session_feed();
        assert!(matches!(
            view.order_undo,
            Some(UndoPoint::Sort(SortOrder::Name))
        ));
        assert_eq!(view.sort_order, SortOrder::Custom);
        view.undo_order_change().unwrap();
        let first = capture().expect("only the sort write");
        assert!(
            matches!(&first.write, ViewWrite::SidebarView(patch) if patch.sort == Some(SortOrder::Name))
        );
        assert!(capture().is_none());

        // A sort queued behind another write answers only for itself.
        view.sort_order = SortOrder::Custom;
        view.order_change_committed(undo(SortOrder::Name), &at);
        first.finish(Err("busy".into()));
        view.apply_session_feed();
        assert_eq!(
            flash(&view).as_deref(),
            Some("Grouping and sort not saved: busy")
        );
        capture()
            .expect("the queued sort")
            .finish(Err("refused".into()));
        view.apply_session_feed();
        assert_eq!(
            flash(&view).as_deref(),
            Some("Order restored, sort not saved: refused (Undo in Ctrl+K retries it)")
        );

        // A sort the queue cannot take stays on offer, as does one a replacement Daemon's
        // frame overtakes while it still shows Custom.
        view.sort_order = SortOrder::Custom;
        view.session_feed.set_mutation_permission_for_test(false);
        view.order_change_committed(undo(SortOrder::Name), &at);
        assert!(matches!(
            view.order_undo,
            Some(UndoPoint::Sort(SortOrder::Name))
        ));
        assert!(flash(&view).is_some_and(|text| text.starts_with("Order restored, sort not saved")));
        view.session_feed.set_mutation_permission_for_test(true);
        view.undo_order_change().unwrap();
        capture().expect("the sort").finish(Ok(RuntimeCursor {
            epoch: "new".into(),
            revision: 50,
        }));
        publish(&mut view, "newer", 1, &[]);
        assert!(matches!(
            view.order_undo,
            Some(UndoPoint::Sort(SortOrder::Name))
        ));
        assert_eq!(view.sort_order, SortOrder::Custom);

        // A sort picked since the move started wins.
        view.sort_order = SortOrder::Newest;
        view.order_change_committed(switch(repositories.clone(), view.sort_changes), &restarted);
        assert!(capture().is_none());
        assert_eq!(view.sort_order, SortOrder::Newest);
        assert!(view.order_undo.is_none());
        assert_eq!(
            flash(&view).as_deref(),
            Some("Order saved, the sort picked since stays")
        );

        // So does one picked away and back before the receipt.
        view.show_sidebar_view(view.group_by, SortOrder::Name);
        let began = view.sort_changes;
        view.show_sidebar_view(view.group_by, SortOrder::Newest);
        view.show_sidebar_view(view.group_by, SortOrder::Name);
        view.order_change_committed(switch(repositories.clone(), began), &restarted);
        assert!(capture().is_none());
        assert_eq!(view.sort_order, SortOrder::Name);
        assert_eq!(
            flash(&view).as_deref(),
            Some("Order saved, the sort picked since stays")
        );
    }

    /// An empty header resolves to the pinned project the renderer shows, not to an archived
    /// session that shares its label.
    #[test]
    #[serial_test::serial]
    fn an_empty_header_names_the_pinned_project_it_shows() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut view = HomeView::new_for_test(
            None,
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let mut archived = Instance::new("old", "/team-a/app");
        archived.archived_at = Some(crate::time::Timestamp::now());
        view.instances.insert(archived.id.clone(), archived);
        let mut pinned = crate::session::Project::new(
            "app",
            "/team-b/app",
            crate::session::ProjectScope::Global,
        );
        pinned.pinned = true;
        view.registered_projects = vec![pinned];
        assert_eq!(
            view.repository_path_for_header("app").as_deref(),
            Some("/team-b/app")
        );
    }

    /// Two repositories sharing a label resolve to the one the renderer seeds the header
    /// from: the newest session's under Name or Newest first, an unplaced one's under Custom.
    #[test]
    #[serial_test::serial]
    fn a_shared_label_names_the_repository_the_sort_shows() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut view = HomeView::new_for_test(
            None,
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let placed = Instance::new("placed", "/a/app");
        let mut unplaced = Instance::new("unplaced", "/b/app");
        unplaced.created_at = placed.created_at - crate::time::Duration::days(1);
        for inst in [placed, unplaced] {
            view.instances.insert(inst.id.clone(), inst);
        }
        view.repository_order = vec!["/a/app".to_string()];
        for (sort, expected) in [
            (SortOrder::Name, "/a/app"),
            (SortOrder::Newest, "/a/app"),
            (SortOrder::Custom, "/b/app"),
        ] {
            view.sort_order = sort;
            assert_eq!(
                view.repository_path_for_header("app").as_deref(),
                Some(expected),
                "{sort:?}"
            );
        }
    }

    /// A Custom header move places only the two headers it swaps; an unplaced one above them
    /// stays unplaced.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_custom_header_move_leaves_unplaced_headers_unplaced() {
        use crate::tui::session_feed::ViewWrite;
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let now = crate::time::Timestamp::now();
        let rows: Vec<Instance> = [("a", 2), ("b", 1), ("c", 0)]
            .into_iter()
            .map(|(name, age)| {
                let mut row = Instance::new(name, &format!("/r/{name}"));
                row.created_at = now - crate::time::Duration::hours(age);
                row
            })
            .collect();
        storage
            .update(|stored, _| {
                *stored = rows.clone();
                Ok(())
            })
            .unwrap();
        crate::session::update_workspace_ordering(|ordering| {
            ordering.repositories = vec!["/r/a".into(), "/r/b".into()];
            Ok(())
        })
        .unwrap();
        let state = native_state(&["reorder"]).await;
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Custom;
        view.group_by = GroupByMode::Project;
        view.flat_items = view.build_flat_items();
        let headers: Vec<&str> = view
            .flat_items
            .iter()
            .filter_map(|item| match item {
                Item::Group { path, depth: 0, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["c", "a", "b"]);
        view.cursor = view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Group { path, .. } if path == "a"))
            .unwrap();
        view.update_selected();
        let mut capture = view.session_feed.view_write_driver_for_test();
        view.move_row_at_cursor(1).unwrap();
        match capture().map(|captured| captured.write) {
            Some(ViewWrite::Ordering(patch)) => {
                assert_eq!(patch.order, None);
                assert_eq!(
                    patch.repositories,
                    Some(vec!["/r/b".to_string(), "/r/a".to_string()])
                );
            }
            other => panic!("expected an order write, got {other:?}"),
        }
    }

    /// A move that names no other group is refused. A carry whose order placement fails
    /// keeps its committed move and reports the failure, leaving the stored order untouched.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_carry_whose_placement_fails_keeps_its_move_and_reports_it() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let rows: Vec<Instance> = [("one", "aaa"), ("two", "bbb")]
            .into_iter()
            .map(|(title, group)| {
                let mut row = Instance::new(title, &format!("/tmp/{title}"));
                row.group_path = group.into();
                row
            })
            .collect();
        let id = rows[0].id.clone();
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = ["aaa", "bbb"]
                    .map(|path| crate::session::Group::new(path, path))
                    .into();
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        // A body with no destination, as an older Client sends for a move within its group,
        // is refused rather than settled as a move.
        let body = serde_json::json!({"target":"session","id":id,"profile":"reorder",
            "source_group":"aaa","direction":"down"});
        let (status, _, value) = request_with_headers(&state, "POST", "/api/reorder", body).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{value}");
        // Headers and siblings are bounded like an order, since the move arranges them under
        // its guards.
        let headers: Vec<String> = (0..=4096).map(|n| format!("g{n}")).collect();
        for body in [
            serde_json::json!({"target":"session","id":id,"profile":"reorder",
                "source_group":"aaa","headers":headers,"direction":"down"}),
            serde_json::json!({"target":"group","group":{"profile":"reorder","path":"aaa"},
                "siblings":headers,"direction":"down"}),
        ] {
            let (status, _, value) =
                request_with_headers(&state, "POST", "/api/reorder", body).await;
            assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE, "{value}");
        }
        let ordering = crate::session::get_app_dir()
            .unwrap()
            .join("workspace-ordering.json");
        std::fs::create_dir_all(&ordering).unwrap();
        let body = serde_json::json!({"target":"session","id":id,"profile":"reorder",
            "source_group":"aaa","destination":"bbb","direction":"down"});
        let (status, _, value) = request_with_headers(&state, "POST", "/api/reorder", body).await;
        assert!(!status.is_success(), "{status}: {value}");
        let (stored, _) = storage.load_with_groups().unwrap();
        let row = stored.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.group_path, "bbb");
        assert!(ordering.is_dir(), "the stored order is untouched");
    }

    /// A carry writes only live workspaces, so a Client rewriting the full order next keeps
    /// every placement it carried, and places none outside its destination.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_carry_places_only_live_workspaces_of_its_destination() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let rows: Vec<Instance> = [("one", "aaa"), ("two", "bbb"), ("three", "ccc")]
            .into_iter()
            .map(|(title, group)| {
                let mut row = Instance::new(title, &format!("/tmp/{title}"));
                row.group_path = group.into();
                row
            })
            .collect();
        let (one, two) = (rows[0].workspace_id(), rows[1].workspace_id());
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = ["aaa", "bbb", "ccc"]
                    .map(|path| crate::session::Group::new(path, path))
                    .into();
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let (status, _, _) = request_with_headers(
            &state,
            "PUT",
            "/api/workspace-ordering",
            serde_json::json!({ "order": ["gone", &one, &two] }),
        )
        .await;
        assert!(status.is_success());
        let body = serde_json::json!({"target":"session","id":rows[0].id,"profile":"reorder",
            "source_group":"aaa","destination":"bbb","direction":"down"});
        let (status, _, value) = request_with_headers(&state, "POST", "/api/reorder", body).await;
        assert!(status.is_success(), "{status}: {value}");
        assert_eq!(
            crate::session::load_workspace_ordering().unwrap().order,
            [one, two]
        );
    }

    /// A Group move conditioned on the sort is stale once the shared sort is another.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_group_move_on_another_sort_is_stale() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let rows: Vec<Instance> = [("one", "aaa"), ("two", "bbb")]
            .into_iter()
            .map(|(title, group)| {
                let mut row = Instance::new(title, &format!("/tmp/{title}"));
                row.group_path = group.into();
                row
            })
            .collect();
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = ["aaa", "bbb"]
                    .map(|path| crate::session::Group::new(path, path))
                    .into();
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        for (base_sort, expected) in [("name", "stale"), ("custom", "moved")] {
            let body = serde_json::json!({"target":"group","group":{"profile":"reorder","path":"aaa"},
                "direction":"down","base_sort":base_sort});
            let (status, _, value) =
                request_with_headers(&state, "POST", "/api/reorder", body).await;
            assert!(status.is_success(), "{status}: {value}");
            assert_eq!(value["status"], expected, "{base_sort}");
        }
    }

    async fn native_move(group: bool, snapshot_first: bool) {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let mut one = Instance::new("one", "/tmp/one");
        one.group_path = "aaa".into();
        let mut two = Instance::new("two", "/tmp/two");
        two.group_path = "bbb".into();
        let id = one.id.clone();
        storage
            .update(|rows, groups| {
                *rows = vec![one.clone(), two.clone()];
                *groups = vec![
                    crate::session::Group::new("aaa", "aaa"),
                    crate::session::Group::new("bbb", "bbb"),
                ];
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Custom;
        view.group_by = GroupByMode::Group;
        view.flat_items = view.build_flat_items();
        if group {
            view.cursor = view
                .flat_items
                .iter()
                .position(|item| matches!(item, Item::Group { path, .. } if path == "aaa"))
                .unwrap();
            view.update_selected();
        } else {
            view.select_session_by_id(&id);
        }
        let before = payload_bytes("reorder");
        let before_ids = view
            .flat_items
            .iter()
            .filter_map(|item| match item {
                Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut respond = view.session_feed.namespace_driver_for_test();
        view.move_row_at_cursor(1).unwrap();
        assert_eq!(payload_bytes("reorder"), before);
        let body = if group {
            serde_json::json!({"target":"group","group":{"profile":"reorder","path":"aaa"},"direction":"down"})
        } else {
            serde_json::json!({"target":"session","id":id,"profile":"reorder","source_group":"aaa","destination":"bbb","direction":"down"})
        };
        let (status, headers, value) =
            request_with_headers(&state, "POST", "/api/reorder", body).await;
        assert!(status.is_success(), "{status}: {value}");
        let receipt = crate::daemon::MutationReceipt {
            cursor: crate::daemon::RuntimeCursor {
                epoch: headers[crate::daemon::RUNTIME_EPOCH_HEADER]
                    .to_str()
                    .unwrap()
                    .into(),
                revision: headers[crate::daemon::RUNTIME_REVISION_HEADER]
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
            },
            outcome: crate::daemon::NamespaceOutcome::Reordered(
                serde_json::from_value(value).unwrap(),
            ),
        };
        let snapshot = published_snapshot(&state).await;
        if snapshot_first {
            view.session_feed.publish_for_test(
                crate::tui::session_feed::SessionFeedResult::Snapshot(std::sync::Arc::new(
                    snapshot.clone(),
                )),
            );
            view.apply_session_feed();
        }
        assert!(matches!(
            respond(Ok(receipt)),
            Some(crate::daemon::NamespaceMutation::Reorder(_))
        ));
        view.apply_session_feed();
        if !snapshot_first {
            assert_eq!(
                view.flat_items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Session { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                before_ids
            );
            view.session_feed.publish_for_test(
                crate::tui::session_feed::SessionFeedResult::Snapshot(std::sync::Arc::new(
                    snapshot,
                )),
            );
            view.apply_session_feed();
        }
        let (rows, groups) = storage.load_with_groups().unwrap();
        if group {
            assert_eq!(
                groups
                    .iter()
                    .map(|group| group.path.as_str())
                    .collect::<Vec<_>>(),
                ["bbb", "aaa"]
            );
            assert_eq!(view.selected_group.as_deref(), Some("aaa"));
            assert!(
                matches!(view.flat_items.get(view.cursor), Some(Item::Group { path, .. }) if path == "aaa")
            );
        } else {
            assert_eq!(
                rows.iter().find(|row| row.id == id).unwrap().group_path,
                "bbb"
            );
            assert_eq!(view.get_instance(&id).unwrap().group_path, "bbb");
            assert_eq!(view.selected_session.as_deref(), Some(id.as_str()));
        }
        assert!(view.info_dialog.is_none());
    }

    /// A move passes the other sessions on its branch, which travel with it, and waits while
    /// a carry is unapplied, since a move built before it shows would undo it.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_move_passes_its_branch_and_waits_for_a_carry() {
        use crate::tui::session_feed::ViewWrite;
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let branch_row = |title: &str| {
            let mut row = Instance::new(title, "/tmp/repo");
            row.group_path = "aaa".into();
            row.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feat".into(),
                main_repo_path: "/tmp/repo".into(),
                managed_by_aoe: true,
                created_at: crate::time::Timestamp::now(),
                base_branch: None,
            });
            row
        };
        let (one, two) = (branch_row("one"), branch_row("two"));
        let mut other = Instance::new("other", "/tmp/other");
        other.group_path = "aaa".into();
        let (branch, other_workspace) = (one.workspace_id(), other.workspace_id());
        storage
            .update(|rows, groups| {
                *rows = vec![one.clone(), two.clone(), other.clone()];
                *groups = vec![crate::session::Group::new("aaa", "aaa")];
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let (status, _, _) = request_with_headers(
            &state,
            "PUT",
            "/api/workspace-ordering",
            serde_json::json!({ "order": [&branch, &other_workspace] }),
        )
        .await;
        assert!(status.is_success());
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Custom;
        view.group_by = GroupByMode::Group;
        view.flat_items = view.build_flat_items();
        let sessions: Vec<String> = view
            .flat_items
            .iter()
            .filter_map(|item| match item {
                Item::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[2], other.id, "the branch's rows lead");
        let mut capture = view.session_feed.view_write_driver_for_test();
        let flash = |view: &HomeView| view.status_flash.as_ref().map(|f| f.text.clone());

        view.select_session_by_id(&sessions[0]);
        view.pending_namespace_intent = Some(NamespaceIntent::ReorderSession {
            id: other.id.clone(),
            switch: None,
        });
        view.move_row_at_cursor(1).unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("The last move is still saving")
        );
        view.status_flash = None;
        view.undo_order_change().unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("The last move is still saving")
        );
        assert!(
            capture().is_none(),
            "nothing is written while the carry is unapplied"
        );
        view.pending_namespace_intent = None;

        // With no other branch above, the move has nowhere to go and does not carry.
        view.select_session_by_id(&sessions[1]);
        view.move_row_at_cursor(-1).unwrap();
        assert_eq!(
            flash(&view).as_deref(),
            Some("Sessions on the same branch move together")
        );
        assert!(capture().is_none());
        assert!(view.pending_namespace_intent.is_none());

        view.select_session_by_id(&sessions[0]);
        view.move_row_at_cursor(1).unwrap();
        match capture().map(|captured| captured.write) {
            Some(ViewWrite::Ordering(patch)) => {
                assert_eq!(patch.order, Some(vec![other_workspace, branch]));
            }
            other => panic!("expected one shared-order write, got {other:?}"),
        }
    }

    /// A workspace whose rows Name splits apart lands on the side it moved to, before the
    /// neighbour moving up and after it moving down, under Name and Newest first alike.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_split_workspace_moves_past_its_neighbour() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let row = |title: &str, branch: &str, age: i64| {
            let mut row = Instance::new(title, "/tmp/repo");
            row.group_path = "aaa".into();
            row.created_at = crate::time::Timestamp::now() - crate::time::Duration::days(age);
            row.worktree_info = Some(crate::session::WorktreeInfo {
                branch: branch.into(),
                main_repo_path: "/tmp/repo".into(),
                managed_by_aoe: true,
                created_at: crate::time::Timestamp::now(),
                base_branch: None,
            });
            row
        };
        // Name and Newest first both show alpha, bravo, charlie.
        let rows = vec![
            row("alpha", "x", 0),
            row("bravo", "y", 1),
            row("charlie", "x", 2),
        ];
        let (x, y) = (rows[0].workspace_id(), rows[1].workspace_id());
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = vec![crate::session::Group::new("aaa", "aaa")];
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        for sort in [SortOrder::Name, SortOrder::Newest] {
            for (title, delta, expected) in [("charlie", -1, [&x, &y]), ("alpha", 1, [&y, &x])] {
                let mut view = HomeView::new_for_test(
                    Some("reorder".into()),
                    crate::tmux::AvailableTools::with_tools(&["claude"]),
                    crate::file_watch::FileWatchService::noop(),
                )
                .unwrap();
                apply_published(&mut view, &state).await;
                view.sort_order = sort;
                view.group_by = GroupByMode::Project;
                view.flat_items = view.build_flat_items();
                let id = rows.iter().find(|r| r.title == title).unwrap().id.clone();
                view.select_session_by_id(&id);
                let mut capture = view.session_feed.namespace_capture_for_test();
                view.move_row_at_cursor(delta).unwrap();
                // Under Repository the freeze carries the nested Group and header orders.
                match capture().map(|(sent, _)| sent) {
                    Some(crate::daemon::NamespaceMutation::Reorder(
                        crate::daemon::ReorderBody::Session {
                            destination: None,
                            headers: Some(headers),
                            freeze: Some(freeze),
                            ..
                        },
                    )) => {
                        assert_eq!(freeze.order, expected.map(String::clone).to_vec());
                        assert_eq!(freeze.repositories, Some(vec!["/tmp/repo".to_string()]));
                        assert_eq!(headers, ["aaa"]);
                    }
                    _ => panic!("{sort:?} {title}: expected a frozen move"),
                }
            }
        }
    }

    /// Under Repository a freeze is refused once two Repositories show the Groups in orders
    /// one profile-wide order cannot keep, as the dashboard refuses it.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_freeze_refuses_repositories_that_disagree_on_the_group_order() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let row = |title: &str, repo: &str, group: &str, age: i64| {
            let mut row = Instance::new(title, repo);
            row.group_path = group.into();
            row.created_at = crate::time::Timestamp::now() - crate::time::Duration::hours(age);
            row
        };
        // Newest first shows alpha above beta under x, and beta above alpha under y.
        let rows = vec![
            row("x1", "/tmp/x", "alpha", 0),
            row("x2", "/tmp/x", "alpha", 1),
            row("x3", "/tmp/x", "beta", 2),
            row("y1", "/tmp/y", "beta", 3),
            row("y2", "/tmp/y", "alpha", 4),
        ];
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = ["alpha", "beta"]
                    .map(|path| crate::session::Group::new(path, path))
                    .into();
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Newest;
        view.group_by = GroupByMode::Project;
        view.flat_items = view.build_flat_items();
        let nested: Vec<&str> = view
            .flat_items
            .iter()
            .filter_map(|item| match item {
                Item::Group { path, depth: 1, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(nested, ["x/alpha", "x/beta", "y/beta", "y/alpha"]);
        let mut capture = view.session_feed.namespace_capture_for_test();
        // A row move and a Repository header move alike.
        for header in [None, Some("x")] {
            match header {
                None => view.select_session_by_id(&rows[0].id),
                Some(label) => {
                    view.cursor = view
                        .flat_items
                        .iter()
                        .position(|item| matches!(item, Item::Group { path, depth: 0, .. } if path == label))
                        .unwrap();
                    view.update_selected();
                }
            }
            view.status_flash = None;
            view.move_row_at_cursor(1).unwrap();
            assert!(capture().is_none(), "{header:?}");
            assert_eq!(
                view.status_flash.as_ref().map(|f| f.text.as_str()),
                Some("Pick Custom first: the Groups can't keep this order"),
                "{header:?}"
            );
            assert_eq!(view.sort_order, SortOrder::Newest);
        }
    }

    /// From Name a Repository header move freezes the nested Group order with the rows and
    /// the header order, in one move.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_repository_header_move_under_name_freezes_its_groups() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let rows: Vec<Instance> = [("x1", "/tmp/x"), ("y1", "/tmp/y")]
            .into_iter()
            .map(|(title, repo)| {
                let mut row = Instance::new(title, repo);
                row.group_path = "alpha".into();
                row
            })
            .collect();
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = vec![crate::session::Group::new("alpha", "alpha")];
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Name;
        view.group_by = GroupByMode::Project;
        view.flat_items = view.build_flat_items();
        view.cursor = view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Group { path, depth: 0, .. } if path == "x"))
            .unwrap();
        view.update_selected();
        let mut capture = view.session_feed.namespace_capture_for_test();
        view.move_row_at_cursor(1).unwrap();
        match capture().map(|(sent, _)| sent) {
            Some(crate::daemon::NamespaceMutation::Reorder(
                crate::daemon::ReorderBody::Session {
                    destination: None,
                    headers: Some(headers),
                    freeze: Some(freeze),
                    ..
                },
            )) => {
                assert_eq!(headers, ["alpha"]);
                assert_eq!(
                    freeze.repositories,
                    Some(vec!["/tmp/y".to_string(), "/tmp/x".to_string()])
                );
                assert_eq!(freeze.order.len(), 2);
            }
            _ => panic!("expected a frozen move"),
        }
    }

    /// A move under Custom places only its header's rows, so other rows stay newest first.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_custom_move_places_only_its_header() {
        use crate::tui::session_feed::ViewWrite;
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let row = |title: &str, group: &str, age: i64| {
            let mut row = Instance::new(title, &format!("/tmp/{title}"));
            row.group_path = group.into();
            row.created_at = crate::time::Timestamp::now() - crate::time::Duration::days(age);
            row
        };
        let rows = vec![
            row("alpha", "aaa", 0),
            row("bravo", "aaa", 1),
            row("charlie", "bbb", 2),
            row("delta", "bbb", 3),
        ];
        storage
            .update(|stored, groups| {
                *stored = rows.clone();
                *groups = ["aaa", "bbb"]
                    .map(|path| crate::session::Group::new(path, path))
                    .into();
                Ok(())
            })
            .unwrap();
        let state = native_state(&["reorder"]).await;
        let mut view = HomeView::new_for_test(
            Some("reorder".into()),
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        apply_published(&mut view, &state).await;
        view.sort_order = SortOrder::Custom;
        view.group_by = GroupByMode::Group;
        view.flat_items = view.build_flat_items();
        view.select_session_by_id(&rows[0].id);
        let mut capture = view.session_feed.view_write_driver_for_test();
        view.move_row_at_cursor(1).unwrap();
        match capture().map(|captured| captured.write) {
            Some(ViewWrite::Ordering(patch)) => assert_eq!(
                patch.order,
                Some(vec![rows[1].workspace_id(), rows[0].workspace_id()])
            ),
            other => panic!("expected an order write, got {other:?}"),
        }
    }

    /// Under Name a header move, a carry and a move within a group commit the order on
    /// screen with the move, and switch the sort to Custom once it shows.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_move_under_name_switches_to_custom_from_the_screen() {
        use crate::tui::session_feed::ViewWrite;
        for case in ["group", "carry", "in place"] {
            let carry = case == "carry";
            let _home = crate::session::test_support::isolate_app_dir();
            let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
            let row = |title: &str, group: &str| {
                let mut row = Instance::new(title, &format!("/tmp/{title}"));
                row.group_path = group.into();
                row
            };
            // The shared order starts newest first and groups are stored ccc, aaa, bbb; Name
            // shows neither.
            let rows = vec![row("one", "aaa"), row("zed", "bbb"), row("bee", "bbb")];
            let (one, zed, bee) = (
                rows[0].workspace_id(),
                rows[1].workspace_id(),
                rows[2].workspace_id(),
            );
            let (id, bee_id) = (rows[0].id.clone(), rows[2].id.clone());
            storage
                .update(|stored, groups| {
                    *stored = rows.clone();
                    *groups = ["ccc", "aaa", "bbb"]
                        .map(|path| crate::session::Group::new(path, path))
                        .into();
                    Ok(())
                })
                .unwrap();
            let state = native_state(&["reorder"]).await;
            let mut view = HomeView::new_for_test(
                Some("reorder".into()),
                crate::tmux::AvailableTools::with_tools(&["claude"]),
                crate::file_watch::FileWatchService::noop(),
            )
            .unwrap();
            apply_published(&mut view, &state).await;
            view.sort_order = SortOrder::Name;
            view.group_by = GroupByMode::Group;
            view.flat_items = view.build_flat_items();
            let body = if carry {
                view.select_session_by_id(&id);
                serde_json::json!({"target":"session","id":id,"profile":"reorder",
                    "source_group":"aaa","destination":"bbb","direction":"down",
                    "headers":["aaa","bbb","ccc"]})
            } else if case == "in place" {
                view.select_session_by_id(&bee_id);
                serde_json::json!({"target":"session","id":bee_id,"profile":"reorder",
                    "source_group":"bbb","direction":"down","headers":["aaa","bbb","ccc"]})
            } else {
                view.cursor = view
                    .flat_items
                    .iter()
                    .position(|item| matches!(item, Item::Group { path, .. } if path == "aaa"))
                    .unwrap();
                view.update_selected();
                serde_json::json!({"target":"group","group":{"profile":"reorder","path":"aaa"},
                    "direction":"down","siblings":["aaa","bbb","ccc"]})
            };
            // The screen order; a carried row enters bbb at its top edge, above bee and zed.
            let expected = if case == "in place" {
                vec![one.clone(), zed.clone(), bee.clone()]
            } else {
                vec![one.clone(), bee.clone(), zed.clone()]
            };
            let mut body = body;
            body["freeze"] = serde_json::json!({
                "order": expected,
                "base": {"order": [], "repositories": []},
                "groups": ["ccc", "aaa", "bbb"],
            });
            // A move built from a row or group order a peer has since replaced is refused
            // untouched.
            for (field, peer) in [
                ("/base/order", serde_json::json!([zed])),
                ("/groups", serde_json::json!(["aaa", "ccc", "bbb"])),
            ] {
                let mut stale = body.clone();
                *stale["freeze"].pointer_mut(field).unwrap() = peer;
                let (status, _, value) =
                    request_with_headers(&state, "POST", "/api/reorder", stale).await;
                assert!(status.is_success(), "{status}: {value}");
                assert_eq!(value["status"], "stale", "{case} {field}");
            }
            assert!(crate::session::load_workspace_ordering()
                .unwrap()
                .order
                .is_empty());

            let mut respond = view.session_feed.namespace_driver_for_test();
            view.move_row_at_cursor(1).unwrap();
            let (status, headers, value) =
                request_with_headers(&state, "POST", "/api/reorder", body.clone()).await;
            assert!(status.is_success(), "{status}: {value}");
            let receipt = crate::daemon::MutationReceipt {
                cursor: receipt_cursor(&headers),
                outcome: crate::daemon::NamespaceOutcome::Reordered(
                    serde_json::from_value(value).unwrap(),
                ),
            };
            match respond(Ok(receipt)) {
                Some(crate::daemon::NamespaceMutation::Reorder(sent)) => {
                    assert_eq!(serde_json::to_value(sent).unwrap(), body);
                }
                _ => panic!("expected the move"),
            }
            assert_eq!(
                crate::session::load_workspace_ordering().unwrap().order,
                expected,
                "the move commits the order on screen with it"
            );
            let mut capture = view.session_feed.view_write_driver_for_test();
            apply_published(&mut view, &state).await;
            match capture().map(|captured| captured.write) {
                Some(ViewWrite::SidebarView(patch)) => {
                    assert_eq!(patch.sort, Some(SortOrder::Custom));
                }
                other => panic!("expected the switch to Custom, got {other:?}"),
            }
            assert_eq!(
                view.status_flash.as_ref().map(|flash| flash.text.as_str()),
                Some("Switched to Custom (Undo in Ctrl+K)")
            );
            assert_eq!(view.sort_order, SortOrder::Custom, "{case}");
            let groups: Vec<String> = view
                .flat_items
                .iter()
                .filter_map(|item| match item {
                    Item::Group { path, .. } if !path.contains("Archived") => Some(path.clone()),
                    _ => None,
                })
                .collect();
            let titles: Vec<String> = view
                .flat_items
                .iter()
                .filter_map(|item| match item {
                    Item::Session { id, .. } => view.get_instance(id).map(|r| r.title.clone()),
                    _ => None,
                })
                .collect();
            if carry {
                assert_eq!(groups, ["aaa", "bbb", "ccc"]);
                assert_eq!(titles, ["one", "bee", "zed"]);
            } else if case == "in place" {
                assert_eq!(groups, ["aaa", "bbb", "ccc"]);
                assert_eq!(titles, ["one", "zed", "bee"]);
            } else {
                assert_eq!(groups, ["bbb", "aaa", "ccc"]);
                assert_eq!(titles, ["bee", "zed", "one"]);
            }

            // Undo puts back the row order and the group order the freeze replaced, in one
            // move guarded by both.
            let stored_groups = || -> Vec<String> {
                let (_, groups) = storage.load_with_groups().unwrap();
                groups.into_iter().map(|group| group.path).collect()
            };
            let frozen_groups = stored_groups();
            let (anchor, anchor_group) = view
                .instances()
                .map(|row| (row.id.clone(), row.group_path.clone()))
                .min()
                .unwrap();
            let undo = serde_json::json!({"target":"session","id":anchor,"profile":"reorder",
                "source_group":anchor_group,"direction":"down","headers":["ccc","aaa","bbb"],
                "freeze":{"order":[],"base":{"order":expected,"repositories":[]},
                    "groups":frozen_groups,"repositories":[]}});
            let mut respond = view.session_feed.namespace_driver_for_test();
            view.undo_order_change().unwrap();
            let (status, headers, value) =
                request_with_headers(&state, "POST", "/api/reorder", undo.clone()).await;
            assert!(status.is_success(), "{status}: {value}");
            assert_eq!(value["status"], "moved", "{case}");
            let receipt = crate::daemon::MutationReceipt {
                cursor: receipt_cursor(&headers),
                outcome: crate::daemon::NamespaceOutcome::Reordered(
                    serde_json::from_value(value).unwrap(),
                ),
            };
            match respond(Ok(receipt)) {
                Some(crate::daemon::NamespaceMutation::Reorder(sent)) => {
                    assert_eq!(serde_json::to_value(sent).unwrap(), undo, "{case}");
                }
                _ => panic!("expected the Undo"),
            }
            assert_eq!(stored_groups(), ["ccc", "aaa", "bbb"], "{case}");
            assert!(crate::session::load_workspace_ordering()
                .unwrap()
                .order
                .is_empty());
            let mut capture = view.session_feed.view_write_driver_for_test();
            apply_published(&mut view, &state).await;
            match capture().map(|captured| captured.write) {
                Some(ViewWrite::SidebarView(patch)) => {
                    assert_eq!(patch.sort, Some(SortOrder::Name), "{case}");
                }
                other => panic!("expected the sort back, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_session_carry_is_receipt_fenced_while_the_runtime_owns_the_rows() {
        for snapshot_first in [false, true] {
            native_move(false, snapshot_first).await;
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn a_group_move_is_receipt_fenced_while_the_runtime_owns_the_rows() {
        for snapshot_first in [false, true] {
            native_move(true, snapshot_first).await;
        }
    }
}

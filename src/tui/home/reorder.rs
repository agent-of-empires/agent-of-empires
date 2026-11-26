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
}

/// What Undo in the command palette does next.
#[derive(Debug)]
pub(super) enum UndoPoint {
    /// Write back `restore` while the Daemon of `epoch` holds the orders the switch left.
    Order {
        restore: OrderUndo,
        order: Vec<String>,
        repositories: Vec<String>,
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
    /// since a move built before it shows would undo it.
    fn refuse_while_a_move_saves(&mut self) -> bool {
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
        if freeze && self.group_by == GroupByMode::Group {
            return self.freeze_in_place(id, order, delta);
        }
        self.submit_custom_order(Some(order), repositories)
    }

    /// Commit `order` with the Groups header order on screen, leaving session `id` in its
    /// group, so the switch to Custom keeps the headers where they are.
    fn freeze_in_place(
        &mut self,
        id: &str,
        order: Vec<String>,
        delta: isize,
    ) -> anyhow::Result<()> {
        let Some(row) = self.instances.get(id) else {
            return self.refresh_after_stale_move();
        };
        let profile = row.source_profile.clone();
        let source_group = row.group_path.clone();
        let headers = self.displayed_headers(&profile);
        let (freeze, switch) = self.frozen_order(order);
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
            Some(UndoPoint::Sort(sort)) => self.restore_sort(sort),
            Some(UndoPoint::Order {
                restore,
                order,
                repositories,
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
                if base.order != order || base.repositories != repositories {
                    self.flash_status("The order changed elsewhere, nothing to undo");
                    return Ok(());
                }
                let change = OrderChange::Undo(restore.clone());
                let written = (restore.order.clone(), restore.repositories.clone());
                // Kept until the write commits, so a refused one can be retried.
                self.order_undo = Some(UndoPoint::Order {
                    restore,
                    order,
                    repositories,
                    epoch,
                });
                return self.write_order(Some(written.0), Some(written.1), Some(change));
            }
        }
        Ok(())
    }

    /// Pick `sort` again for Undo, retried from the palette when the write fails.
    fn restore_sort(&mut self, sort: SortOrder) {
        let behind = self.session_feed.sidebar_view_pending();
        self.apply_sort_order(sort);
        if self.session_feed.sidebar_view_pending() {
            self.sort_restore = Some(SortRestore { sort, behind });
        }
    }

    /// Act on a settled sidebar view write. A failed one carrying Undo's sort leaves the
    /// sort to retry.
    pub(super) fn sidebar_view_settled(&mut self, result: &Result<(), String>) {
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
            (Ok(()), _) => {}
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
                base: snapshot.is_some().then_some(base),
            })?;
        if !matches!(change, Some(OrderChange::Undo(_))) {
            self.order_undo = None;
        }
        self.pending_order_change = change;
        Ok(())
    }

    /// The order a move under Name or Newest first commits with the move, and the switch to
    /// Custom that follows it.
    fn frozen_order(&self, order: Vec<String>) -> (crate::daemon::FrozenOrder, OrderChange) {
        let sessions = self
            .session_feed
            .applied_snapshot()
            .map(|snapshot| snapshot.contents.sessions.clone())
            .unwrap_or_default();
        let order = crate::server::api::sessions::writable_workspace_order(&sessions, order);
        let base = self.ordering_base();
        let change = OrderChange::SwitchToCustom {
            undo: self.order_undo_point(),
            expected: (order.clone(), base.repositories.clone()),
        };
        (crate::daemon::FrozenOrder { order, base }, change)
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
            OrderChange::SwitchToCustom { undo, expected } => {
                let base = self.ordering_base();
                let kept = self
                    .session_feed
                    .applied_snapshot()
                    .is_some_and(|snapshot| snapshot.cursor == *receipt)
                    && (&base.order, &base.repositories) == (&expected.0, &expected.1);
                // A sort picked since the move started wins.
                if self.sort_order != undo.sort && self.sort_order != SortOrder::Custom {
                    self.order_undo = None;
                    self.flash_status("Order saved, the sort picked since stays");
                    return;
                }
                if self.sort_order != SortOrder::Custom {
                    self.apply_sort_order(SortOrder::Custom);
                }
                self.order_undo = kept.then_some(UndoPoint::Order {
                    restore: undo,
                    order: expected.0,
                    repositories: expected.1,
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
                if self.sort_order == SortOrder::Custom && undo.sort != SortOrder::Custom {
                    self.restore_sort(undo.sort);
                }
                self.flash_status("Order change undone");
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
    /// `build_flat_items_by_project` ranks them, is the one a move rewrites.
    fn repository_path_for_header(&self, label: &str) -> Option<String> {
        if crate::session::is_synthetic_project_header(label) || label.contains('/') {
            return None;
        }
        let rank = |repo: &str| {
            self.repository_order
                .iter()
                .position(|r| r.trim_end_matches('/') == repo)
        };
        self.instances
            .values()
            .filter(|inst| !inst.scratch && project_group_key(inst) == label)
            .map(|inst| {
                let repo = inst.repo_path().trim_end_matches('/').to_string();
                let live = !inst.is_archived() && !inst.is_trashed();
                let rank = rank(&repo);
                (
                    (
                        !live,
                        rank.is_some(),
                        rank,
                        std::cmp::Reverse(inst.created_at),
                    ),
                    repo,
                )
            })
            .min()
            .map(|(_, repo)| repo)
            .or_else(|| {
                self.registered_projects
                    .iter()
                    .find(|p| crate::session::projects::repo_label(&p.path) == label)
                    .map(|p| p.path.trim_end_matches('/').to_string())
            })
    }

    fn move_header_row(&mut self, path: &str, delta: isize) -> anyhow::Result<()> {
        if self.group_by == GroupByMode::Group {
            return self.move_group_row(path, delta);
        }
        let Some(repo) = self.repository_path_for_header(path) else {
            self.flash_status("Only repository headers move under the Repository grouping");
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
            self.frozen_order(order)
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
    fn move_group_row(&mut self, group_path: &str, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        let group = self.canonical_group_location(group_path)?;
        let freeze = self.sort_order != SortOrder::Custom;
        let siblings = freeze.then(|| self.displayed_sibling_groups(&group));
        let (freeze, switch) = freeze
            .then(|| self.frozen_order(self.displayed_workspace_order()))
            .unzip();
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Group {
                group,
                direction: move_direction(delta),
                siblings,
                freeze,
            }),
            NamespaceIntent::ReorderGroup { switch },
        )
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
        let switch = |expected: Vec<String>| OrderChange::SwitchToCustom {
            undo: OrderUndo {
                order: Vec::new(),
                repositories: Vec::new(),
                sort: SortOrder::Name,
            },
            expected: (Vec::new(), expected),
        };
        let at = publish(&mut view, "e", 2, &["/r/b", "/r/a"]);
        let earlier = RuntimeCursor {
            revision: 1,
            ..at.clone()
        };
        for (receipt, expected) in [(&earlier, repositories.clone()), (&at, Vec::new())] {
            view.order_change_committed(switch(expected), receipt);
            assert!(view.order_undo.is_none(), "{receipt:?}");
            assert_eq!(flash(&view).as_deref(), Some("Switched to Custom"));
        }
        view.order_change_committed(switch(repositories.clone()), &at);
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
            switch(repositories.clone()),
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
        view.order_change_committed(switch(repositories.clone()), &restarted);
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

        // A sort picked since the move started wins.
        view.sort_order = SortOrder::Newest;
        view.order_change_committed(switch(repositories.clone()), &restarted);
        assert!(capture().is_none());
        assert_eq!(view.sort_order, SortOrder::Newest);
        assert!(view.order_undo.is_none());
        assert_eq!(
            flash(&view).as_deref(),
            Some("Order saved, the sort picked since stays")
        );
    }

    /// A carry whose order placement fails, or that names no other group, leaves the row in
    /// its source group, so the move can be made again.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_carry_whose_placement_fails_does_not_move_the_row() {
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
        // leaves the row where it is.
        let body = serde_json::json!({"target":"session","id":id,"profile":"reorder",
            "source_group":"aaa","direction":"down"});
        let (status, _, value) = request_with_headers(&state, "POST", "/api/reorder", body).await;
        assert!(status.is_success(), "{status}: {value}");
        assert_eq!(value["status"], "stale");
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
        assert_eq!(row.group_path, "aaa");
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
        use crate::tui::session_feed::ViewWrite;
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
                let mut capture = view.session_feed.view_write_driver_for_test();
                view.move_row_at_cursor(delta).unwrap();
                match capture().map(|captured| captured.write) {
                    Some(ViewWrite::Ordering(patch)) => {
                        assert_eq!(patch.order, Some(expected.map(String::clone).to_vec()));
                    }
                    other => panic!("{sort:?} {title}: expected an order write, got {other:?}"),
                }
            }
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
            });
            // A move built from an order a peer has since replaced is refused untouched.
            let mut stale = body.clone();
            stale["freeze"]["base"]["order"] = serde_json::json!([zed]);
            let (status, _, value) =
                request_with_headers(&state, "POST", "/api/reorder", stale).await;
            assert!(status.is_success(), "{status}: {value}");
            assert_eq!(value["status"], "stale", "{case}");
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

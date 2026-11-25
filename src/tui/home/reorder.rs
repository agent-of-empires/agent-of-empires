//! Moving rows by hand. A row moves within its header by rewriting the shared Custom
//! order (`PUT /api/workspace-ordering`), the order the web dashboard's drag writes too.
//! Under My groups a row at the edge of its group crosses into the next one.

use super::*;
use crate::session::config::{GroupByMode, SortOrder};

impl HomeView {
    /// Move the cursor's row one slot in `delta` (-1 up, 1 down). Under the Name sort the
    /// list switches to Custom, starting from the order on screen.
    pub(super) fn move_row_at_cursor(&mut self, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        // Each move is built from the applied order, so wait until the last one shows.
        if self.session_feed.ordering_pending() {
            self.flash_status("The last move is still saving");
            return Ok(());
        }
        if let Some(path) = self.selected_group.clone() {
            return self.move_header_row(&path, delta);
        }
        let Some(id) = self.selected_session.clone() else {
            return Ok(());
        };
        let Some(at) = self.session_row(&id) else {
            return Ok(());
        };
        match self.sibling_session(at, delta) {
            Some(neighbour) => self.swap_workspaces(&id, &neighbour),
            None if self.group_by == GroupByMode::Group => self.carry_session(&id, delta),
            None => Ok(()),
        }
    }

    /// The session row next to `at` under the same header, if any. A header's own sessions
    /// come before its child headers, so a header at the row's depth ends the run.
    fn sibling_session(&self, at: usize, delta: isize) -> Option<String> {
        let depth = self.flat_items.get(at)?.depth();
        let mut cursor = at;
        loop {
            cursor = cursor.checked_add_signed(delta)?;
            match self.flat_items.get(cursor)? {
                Item::Session { id, depth: d } if *d == depth => return Some(id.clone()),
                Item::Session { depth: d, .. } if *d > depth => continue,
                Item::Group { depth: d, .. } if *d > depth => continue,
                _ => return None,
            }
        }
    }

    /// Swap two rows' workspaces in the shared order, or in the order on screen when the
    /// list is sorted by Name, which then switches to Custom.
    fn swap_workspaces(&mut self, id: &str, neighbour: &str) -> anyhow::Result<()> {
        let workspace = |view: &Self, id: &str| view.get_instance(id).map(Instance::workspace_id);
        let (Some(a), Some(b)) = (workspace(self, id), workspace(self, neighbour)) else {
            return self.refresh_after_stale_move();
        };
        if a == b {
            self.flash_status("Sessions on the same branch move together");
            return Ok(());
        }
        let freeze = self.sort_order != SortOrder::Custom;
        let mut order = if freeze {
            self.displayed_workspace_order()
        } else {
            self.shared_workspace_order()
        };
        for id in [&a, &b] {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
        let (i, j) = (
            order.iter().position(|w| *w == a).unwrap(),
            order.iter().position(|w| *w == b).unwrap(),
        );
        order.swap(i, j);
        let repositories = (freeze && self.group_by == GroupByMode::Project)
            .then(|| keep_unshown(self.displayed_repository_order(), &self.repository_order));
        self.submit_custom_order(Some(order), repositories)
    }

    /// Write a Custom order. One taken from the screen switches the sort to Custom once it
    /// commits, so a refused write leaves the sort alone.
    fn submit_custom_order(
        &mut self,
        order: Option<Vec<String>>,
        repositories: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        self.session_feed
            .submit_workspace_ordering(crate::daemon::WorkspaceOrderingPatch {
                order,
                repositories,
            })?;
        self.custom_after_ordering = self.sort_order != SortOrder::Custom;
        Ok(())
    }

    /// Clear the shared Custom order, so rows and headers sit newest first again.
    pub(super) fn reset_custom_order(&mut self) -> anyhow::Result<()> {
        // A pending move may switch the sort to Custom once its own write commits.
        if self.session_feed.ordering_pending() {
            self.flash_status("The last move is still saving");
            return Ok(());
        }
        self.session_feed
            .submit_workspace_ordering(crate::daemon::WorkspaceOrderingPatch {
                order: Some(Vec::new()),
                repositories: Some(Vec::new()),
            })?;
        self.flash_status("Custom order reset to newest first");
        Ok(())
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

    /// The repository path behind a top-level Repository header, keyed by its label.
    fn repository_path_for_header(&self, label: &str) -> Option<String> {
        if crate::session::is_synthetic_project_header(label) || label.contains('/') {
            return None;
        }
        self.instances
            .values()
            .find(|inst| !inst.scratch && project_group_key(inst) == label)
            .map(|inst| inst.repo_path().trim_end_matches('/').to_string())
            .or_else(|| {
                self.registered_projects
                    .iter()
                    .find(|p| crate::session::projects::repo_label(&p.path) == label)
                    .map(|p| p.path.trim_end_matches('/').to_string())
            })
    }

    fn move_header_row(&mut self, path: &str, delta: isize) -> anyhow::Result<()> {
        if self.group_by == GroupByMode::Group {
            if self.sort_order != SortOrder::Custom {
                self.flash_status("Pick the Custom sort (o) to arrange groups by hand");
                return Ok(());
            }
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
        order.swap(at, to);
        let rows = (self.sort_order != SortOrder::Custom).then(|| self.displayed_workspace_order());
        let order = keep_unshown(order, &self.repository_order);
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
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Session {
                id: id.to_owned(),
                profile,
                source_group,
                destination,
                direction: move_direction(delta),
            }),
            NamespaceIntent::ReorderSession { id: id.to_owned() },
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

    fn move_group_row(&mut self, group_path: &str, delta: isize) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        let group = self.canonical_group_location(group_path)?;
        self.submit_namespace_intent(
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Group {
                group,
                direction: move_direction(delta),
            }),
            NamespaceIntent::ReorderGroup,
        )
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
fn keep_unshown(mut shown: Vec<String>, persisted: &[String]) -> Vec<String> {
    let mut previous: Option<&String> = None;
    for entry in persisted {
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
        apply_published, native_state, payload_bytes, published_snapshot, request_with_headers,
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
        ] {
            assert_eq!(
                keep_unshown(list(shown), &list(persisted)),
                list(expected),
                "{shown:?} over {persisted:?}"
            );
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

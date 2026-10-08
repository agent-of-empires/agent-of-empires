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
    pub(super) fn move_row_at_cursor(&mut self, delta: isize) -> anyhow::Result<()> {
        if self.sort_order != SortOrder::Custom {
            self.flash_status("Press o for the Custom sort to arrange rows by hand");
            return Ok(());
        }
        if self.group_by != GroupByMode::Manual {
            self.flash_status("Rows are arranged by hand in Manual grouping only (g)");
            return Ok(());
        }
        if let Some(path) = self.selected_group.clone() {
            self.move_group_row(&path, delta)?;
        } else if let Some(id) = self.selected_session.clone() {
            self.move_session_row(&id, delta)?;
        }
        Ok(())
    }

    fn move_session_row(&mut self, id: &str, delta: isize) -> anyhow::Result<()> {
        self.submit_session_reorder(id, delta, None)
    }

    pub(super) fn submit_session_reorder(
        &mut self,
        id: &str,
        delta: isize,
        destination: Option<String>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(delta == -1 || delta == 1, "Invalid row direction");
        let row = self
            .instances
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("The selected row changed elsewhere"))?;
        let profile = row.source_profile.clone();
        let source_group = row.group_path.clone();
        let crossing = destination.is_some();
        let continuation_destination = if crossing {
            None
        } else {
            let groups = self.displayed_group_order(Some(&profile));
            groups
                .iter()
                .position(|group| *group == source_group)
                .and_then(|at| at.checked_add_signed(delta))
                .and_then(|at| groups.get(at))
                .map(|path| (*path).to_owned())
        };
        let mutation =
            crate::daemon::NamespaceMutation::Reorder(crate::daemon::ReorderBody::Session {
                id: id.to_owned(),
                profile: profile.clone(),
                source_group: source_group.clone(),
                direction: if delta < 0 {
                    crate::daemon::MoveDirection::Up
                } else {
                    crate::daemon::MoveDirection::Down
                },
                destination,
            });
        self.submit_namespace_intent(
            mutation,
            NamespaceIntent::ReorderSession {
                id: id.to_owned(),
                profile,
                source_group,
                delta,
                crossing,
                continuation_destination,
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
                direction: if delta < 0 {
                    crate::daemon::MoveDirection::Up
                } else {
                    crate::daemon::MoveDirection::Down
                },
            }),
            NamespaceIntent::ReorderGroup,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::home::tests::{
        apply_published, native_state, payload_bytes, published_snapshot, request_with_headers,
    };

    async fn native_move(group: bool, snapshot_first: bool) {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("reorder").unwrap();
        let mut one = Instance::new("one", "/tmp/one");
        one.group_path = "aaa".into();
        one.sort_index = Some(0);
        let mut two = Instance::new("two", "/tmp/two");
        two.group_path = if group { "bbb" } else { "aaa" }.into();
        two.sort_index = Some(1);
        let id = one.id.clone();
        storage
            .update(|rows, groups| {
                *rows = vec![one.clone(), two.clone()];
                *groups = vec![crate::session::Group::new("aaa", "aaa")];
                if group {
                    groups.push(crate::session::Group::new("bbb", "bbb"));
                }
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
        view.group_by = GroupByMode::Manual;
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
            serde_json::json!({"target":"session","id":id,"profile":"reorder","source_group":"aaa","direction":"down"})
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
            assert!(
                rows.iter().find(|row| row.id == id).unwrap().sort_index
                    > rows.iter().find(|row| row.id == two.id).unwrap().sort_index
            );
            assert_eq!(view.selected_session.as_deref(), Some(id.as_str()));
            let displayed = view
                .flat_items
                .iter()
                .filter_map(|item| match item {
                    Item::Session { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(displayed, [two.id.as_str(), id.as_str()]);
        }
        assert!(view.info_dialog.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_row_move_is_receipt_fenced_while_the_runtime_owns_the_rows() {
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

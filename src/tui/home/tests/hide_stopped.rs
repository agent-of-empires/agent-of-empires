//! `y` hides stopped sessions inside groups and shows `shown/total` on their headers.

use super::*;
use crate::session::config::{GroupByMode, SortOrder};
use crate::session::Status;

fn with_status(mut inst: Instance, status: Status) -> Instance {
    inst.status = status;
    inst
}

/// `util`: one idle session and two stopped; `ops`: two stopped; one stopped session in no group.
fn env_with_stopped(manual: bool) -> TestEnv {
    let instances = [
        with_status(instance_in("util-live", "/tmp/util", "util"), Status::Idle),
        with_status(
            instance_in("util-stopped-a", "/tmp/util", "util"),
            Status::Stopped,
        ),
        with_status(
            instance_in("util-stopped-b", "/tmp/util", "util"),
            Status::Stopped,
        ),
        with_status(
            instance_in("ops-stopped-a", "/tmp/ops", "ops"),
            Status::Stopped,
        ),
        with_status(
            instance_in("ops-stopped-b", "/tmp/ops", "ops"),
            Status::Stopped,
        ),
        with_status(
            Instance::new("loose-stopped", "/tmp/loose"),
            Status::Stopped,
        ),
    ];
    seeded_env(test_home(), &instances, manual)
}

fn session_titles(view: &HomeView) -> Vec<String> {
    view.flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Session { id, .. } => view.get_instance(id).map(|i| i.title.clone()),
            Item::Group { .. } => None,
        })
        .collect()
}

fn header_text(view: &HomeView, group: &str) -> String {
    let item = view
        .flat_items
        .iter()
        .find(|item| matches!(item, Item::Group { path, .. } if path == group))
        .unwrap_or_else(|| panic!("no {group} header"));
    rendered_row_text(view, item)
}

fn press_y(env: &mut TestEnv) {
    {
        let result = env.view.handle_key(key(KeyCode::Char('y')), None);
        drain_persistence(&mut env.view).unwrap();
        result
    };
}

#[test]
#[serial]
fn y_hides_stopped_sessions_in_groups_and_counts_them_on_the_header() {
    let mut env = env_with_stopped(true);
    assert_eq!(session_titles(&env.view).len(), 6);
    assert!(header_text(&env.view, "util").contains("util (3)"));

    press_y(&mut env);
    let shown = session_titles(&env.view);
    assert!(shown.contains(&"util-live".to_string()));
    assert!(
        shown.contains(&"loose-stopped".to_string()),
        "a session in no group is never hidden"
    );
    assert!(shown.iter().all(|t| !t.contains("-stopped-")), "{shown:?}");
    assert!(header_text(&env.view, "util").contains("util (1/3)"));
    assert!(
        header_text(&env.view, "ops").contains("ops (0/2)"),
        "a group whose sessions are all hidden keeps its header"
    );

    press_y(&mut env);
    assert_eq!(session_titles(&env.view).len(), 6);
    assert!(header_text(&env.view, "util").contains("util (3)"));
}

/// Project grouping derives its groups from the repo, so hiding follows those groups too.
#[test]
#[serial]
fn project_grouping_hides_by_its_own_groups() {
    let mut env = env_with_stopped(false);
    env.view.group_by = GroupByMode::Project;
    env.view.rebuild_flat_items();
    assert_eq!(session_titles(&env.view).len(), 6);

    press_y(&mut env);
    assert_eq!(session_titles(&env.view), vec!["util-live".to_string()]);
    assert!(header_text(&env.view, "util").contains("util (1/3)"));
    assert!(header_text(&env.view, "loose").contains("loose (0/1)"));
}

/// Attention sort lists sessions without groups, and archived sessions keep their own section,
/// so neither loses a row.
#[test]
#[serial]
fn attention_sort_and_the_archived_section_hide_nothing() {
    let mut archived = with_status(
        instance_in("util-archived", "/tmp/util", "util"),
        Status::Stopped,
    );
    archived.archived_at = Some(chrono::Utc::now());
    let mut env = seeded_env(
        test_home(),
        &[
            with_status(instance_in("util-live", "/tmp/util", "util"), Status::Idle),
            with_status(
                instance_in("util-stopped", "/tmp/util", "util"),
                Status::Stopped,
            ),
            archived,
        ],
        true,
    );
    env.view.archived_section_collapsed = false;
    env.view.rebuild_flat_items();

    press_y(&mut env);
    let shown = session_titles(&env.view);
    assert!(!shown.contains(&"util-stopped".to_string()), "{shown:?}");
    assert!(
        shown.contains(&"util-archived".to_string()),
        "the Archived section is not a group to hide in: {shown:?}"
    );

    env.view.apply_sort_order(SortOrder::Attention);
    let shown = session_titles(&env.view);
    assert!(
        shown.contains(&"util-stopped".to_string()),
        "Attention sort has no groups: {shown:?}"
    );
}

/// The cursor on a session that gets hidden lands on a row that is still there.
#[test]
#[serial]
fn hiding_the_selected_session_moves_the_selection_to_a_shown_row() {
    let mut env = env_with_stopped(true);
    let row = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if env.view.get_instance(id).is_some_and(|i| i.title == "util-stopped-a")))
        .expect("util-stopped-a row");
    env.view.cursor = row;
    env.view.update_selected();

    press_y(&mut env);
    assert_eq!(env.view.selected_session, None);
    assert_eq!(env.view.selected_group.as_deref(), Some("util"));
    assert!(matches!(
        &env.view.flat_items[env.view.cursor],
        Item::Group { path, .. } if path == "util"
    ));
}

/// Rows above the selection can drop out in the same rebuild, as when a reload brings in
/// sessions stopped elsewhere; the selection still lands on its own group's header.
#[test]
#[serial]
fn the_selection_finds_its_own_header_when_rows_above_it_also_drop_out() {
    let mut env = seeded_env(
        test_home(),
        &[
            with_status(
                instance_in("first-live", "/tmp/first", "first"),
                Status::Idle,
            ),
            with_status(
                instance_in("first-other", "/tmp/first", "first"),
                Status::Idle,
            ),
            with_status(
                instance_in("second-live", "/tmp/second", "second"),
                Status::Idle,
            ),
            with_status(
                instance_in("second-other", "/tmp/second", "second"),
                Status::Idle,
            ),
            with_status(
                instance_in("third-live", "/tmp/third", "third"),
                Status::Idle,
            ),
        ],
        true,
    );
    // Alphabetical, so hiding a group's only live session cannot also reorder the headers.
    env.view.apply_sort_order(SortOrder::AZ);
    press_y(&mut env);
    let selected = select_session(&mut env, "second-live");
    let upper_live = env
        .view
        .instances
        .values()
        .find(|i| i.title == "first-live")
        .map(|i| i.id.clone())
        .unwrap();

    for id in [&upper_live, &selected] {
        env.view
            .mutate_instance(id, |inst| inst.status = Status::Stopped);
    }
    env.view.rebuild_flat_items_keeping_cursor();
    assert_eq!(env.view.selected_group.as_deref(), Some("second"));
}

#[test]
#[serial]
fn strict_mode_toggles_on_shift_y() {
    let mut env = env_with_stopped(true);
    env.view.strict_hotkeys = true;
    {
        let result = env.view.handle_key(key(KeyCode::Char('Y')), None);
        drain_persistence(&mut env.view).unwrap();
        result
    };
    assert!(header_text(&env.view, "util").contains("util (1/3)"));
}

fn select_session(env: &mut TestEnv, title: &str) -> String {
    let (row, id) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(row, item)| match item {
            Item::Session { id, .. }
                if env.view.get_instance(id).is_some_and(|i| i.title == title) =>
            {
                Some((row, id.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {title} row"));
    env.view.cursor = row;
    env.view.update_selected();
    id
}

/// A session stopped while hiding is on leaves the list at once, and the selection stays with
/// its group instead of jumping to the row that slid up. Starting it again brings it back.
#[test]
#[serial]
fn a_session_crossing_into_or_out_of_stopped_follows_the_toggle_at_once() {
    let mut env = env_with_stopped(true);
    press_y(&mut env);
    let live = select_session(&mut env, "util-live");

    env.view.set_instance_status(&live, Status::Stopped);
    assert!(!session_titles(&env.view).contains(&"util-live".to_string()));
    assert!(header_text(&env.view, "util").contains("util (0/3)"));
    assert_eq!(env.view.selected_group.as_deref(), Some("util"));
    assert_eq!(env.view.selected_session, None);

    env.view.set_instance_status(&live, Status::Idle);
    assert!(session_titles(&env.view).contains(&"util-live".to_string()));
    assert!(header_text(&env.view, "util").contains("util (1/3)"));
}

/// A move renumbers every sibling in the store, hidden ones included, so it is refused while
/// they are hidden.
#[test]
#[serial]
fn custom_moves_wait_until_stopped_sessions_are_shown() {
    let mut env = env_with_stopped(true);
    env.view.apply_sort_order(SortOrder::Custom);
    press_y(&mut env);
    select_session(&mut env, "util-live");
    let before: Vec<_> = env
        .view
        .instances
        .values()
        .map(|i| (i.id.clone(), i.sort_index, i.group_path.clone()))
        .collect();

    {
        let submitted = env.view.move_row_at_cursor(1);
        await_transaction_result(&mut env.view, submitted)
    }
    .unwrap();
    assert!(env
        .view
        .status_flash_text()
        .is_some_and(|t| t.contains("Show stopped sessions")));
    let after: Vec<_> = env
        .view
        .instances
        .values()
        .map(|i| (i.id.clone(), i.sort_index, i.group_path.clone()))
        .collect();
    assert_eq!(before, after);

    let group_row = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "util"))
        .unwrap();
    env.view.cursor = group_row;
    env.view.update_selected();
    env.view.status_flash = None;
    {
        let submitted = env.view.move_row_at_cursor(1);
        await_transaction_result(&mut env.view, submitted)
    }
    .unwrap();
    assert!(
        !env.view
            .status_flash_text()
            .is_some_and(|t| t.contains("Show stopped sessions")),
        "a group header moves among groups, which hiding does not touch"
    );

    press_y(&mut env);
    select_session(&mut env, "util-live");
    {
        let submitted = env.view.move_row_at_cursor(-1);
        await_transaction_result(&mut env.view, submitted)
    }
    .unwrap();
    let moved: Vec<_> = env
        .view
        .instances
        .values()
        .map(|i| (i.id.clone(), i.sort_index, i.group_path.clone()))
        .collect();
    assert_ne!(
        before, moved,
        "with everything shown the same move goes through"
    );
}

/// A parent group counts its subgroups' sessions, shown and total alike.
#[test]
#[serial]
fn a_parent_header_counts_its_subgroups() {
    let mut env = seeded_env(
        test_home(),
        &[
            with_status(instance_in("util-live", "/tmp/util", "util"), Status::Idle),
            with_status(
                instance_in("sub-live", "/tmp/sub", "util/sub"),
                Status::Idle,
            ),
            with_status(
                instance_in("sub-stopped", "/tmp/sub", "util/sub"),
                Status::Stopped,
            ),
        ],
        true,
    );
    press_y(&mut env);
    assert!(header_text(&env.view, "util").contains("util (2/3)"));
    assert!(header_text(&env.view, "util/sub").contains("sub (1/2)"));
}

/// The all-profiles list can hold the same group path twice; each header counts its own profile.
#[test]
#[serial]
fn each_profile_counts_its_own_copy_of_a_group() {
    let (_temp, _guard) = test_home();
    seed_profile(
        "alpha",
        &[
            with_status(instance_in("a-live", "/tmp/a", "util"), Status::Idle),
            with_status(instance_in("a-stopped", "/tmp/a", "util"), Status::Stopped),
        ],
    );
    seed_profile(
        "beta",
        &[
            with_status(
                instance_in("b-stopped-1", "/tmp/b", "util"),
                Status::Stopped,
            ),
            with_status(
                instance_in("b-stopped-2", "/tmp/b", "util"),
                Status::Stopped,
            ),
            with_status(
                instance_in("b-stopped-3", "/tmp/b", "util"),
                Status::Stopped,
            ),
        ],
    );
    let mut view = test_view(None);
    view.group_by = GroupByMode::Manual;
    view.rebuild_flat_items();
    view.update_selected();
    {
        let result = view.handle_key(key(KeyCode::Char('y')), None);
        drain_persistence(&mut view).unwrap();
        result
    };

    let header_for = |profile: &str| {
        let item = view
            .flat_items
            .iter()
            .find(|item| {
                matches!(item, Item::Group { path, profile: p, .. }
                    if path == "util" && p.as_deref() == Some(profile))
            })
            .unwrap_or_else(|| panic!("no util header for {profile}"));
        rendered_row_text(&view, item)
    };
    assert!(
        header_for("alpha").contains("util (1/2)"),
        "{}",
        header_for("alpha")
    );
    assert!(
        header_for("beta").contains("util (0/3)"),
        "{}",
        header_for("beta")
    );
}

#[test]
#[serial]
fn the_toggle_says_what_it_did() {
    let mut env = env_with_stopped(true);
    press_y(&mut env);
    assert!(env
        .view
        .status_flash_text()
        .is_some_and(|t| t.contains("hidden")));
    press_y(&mut env);
    assert!(env
        .view
        .status_flash_text()
        .is_some_and(|t| t.contains("Showing")));
}

/// A hidden structured session the daemon reports working again comes back into its group.
#[test]
#[serial]
fn a_structured_session_lifted_out_of_stopped_reappears() {
    let mut acp = with_status(
        instance_in("acp-stopped", "/tmp/util", "util"),
        Status::Stopped,
    );
    acp.tool = "claude".into();
    acp.view = crate::session::View::Structured;
    let id = acp.id.clone();
    let mut env = seeded_env(
        test_home(),
        &[
            with_status(instance_in("util-live", "/tmp/util", "util"), Status::Idle),
            acp,
        ],
        true,
    );
    press_y(&mut env);
    assert!(!session_titles(&env.view).contains(&"acp-stopped".to_string()));

    env.view
        .apply_daemon_status_update(crate::tui::session_feed::DaemonStatusUpdate {
            id,
            status: Status::Running,
            last_error: None,
            last_accessed_at: None,
            idle_entered_at: None,
            pending_approvals: Vec::new(),
        });
    assert!(session_titles(&env.view).contains(&"acp-stopped".to_string()));
    assert!(header_text(&env.view, "util").contains("util (2)"));
}

/// A session menu closes when its session stops and drops out under it, rather than acting on the
/// group header the selection falls back to.
#[test]
#[serial]
fn a_session_menu_closes_when_its_session_is_hidden() {
    let mut env = env_with_stopped(true);
    press_y(&mut env);
    let id = select_session(&mut env, "util-live");
    env.view.list_inner_area = ratatui::layout::Rect::new(1, 1, 28, 20);
    env.view.list_area = ratatui::layout::Rect::new(0, 0, 30, 22);
    assert!(env
        .view
        .handle_right_click(5, env.view.list_inner_area.y + env.view.cursor as u16));
    assert_eq!(env.view.selected_session.as_deref(), Some(id.as_str()));
    assert!(env.view.context_menu.is_some());

    env.view.set_instance_status(&id, Status::Stopped);

    assert_eq!(env.view.selected_group.as_deref(), Some("util"));
    assert!(env.view.context_menu.is_none());
}

/// Live-send on a session's terminal ends when the session is stopped elsewhere and hiding takes
/// its row away: the terminal can outlive the agent, and keys must not reach a pane no longer
/// shown.
#[test]
#[serial]
fn live_send_ends_when_its_session_is_hidden() {
    let mut env = env_with_stopped(true);
    press_y(&mut env);
    let id = select_session(&mut env, "util-live");
    live_send_to_terminal(&mut env, &id);

    Storage::new_unwatched("test")
        .unwrap()
        .update(|insts, _| {
            let disk = insts.iter_mut().find(|i| i.id == id).unwrap();
            disk.lifecycle_generation += 1;
            disk.status = Status::Stopped;
            Ok(())
        })
        .unwrap();
    {
        env.view.request_reload(super::super::ReloadKind::Storage);
        drain_persistence(&mut env.view)
    }
    .unwrap();

    assert!(!session_titles(&env.view).contains(&"util-live".to_string()));
    assert!(env.view.live_send.is_none(), "live-send ends with its row");
    assert_eq!(env.view.selected_group.as_deref(), Some("util"));
}

fn live_send_to_terminal(env: &mut TestEnv, id: &str) {
    let title = env.view.get_instance(id).unwrap().title.clone();
    let mut state = live_send_state(
        id,
        &title,
        &crate::tmux::TerminalSession::resolve_name(id, &title),
    );
    state.target = crate::tui::home::live_send::LiveSendTarget::Terminal;
    env.view.live_send = Some(state);
}

/// Live-send on a stopped session's terminal ends when a sort change (as the sort picker
/// submits it) moves the list out of Attention and the filter hides the session.
#[test]
#[serial]
fn live_send_ends_when_a_sort_change_hides_its_session() {
    let mut env = env_with_stopped(true);
    env.view.apply_sort_order(SortOrder::Attention);
    press_y(&mut env);
    let id = select_session(&mut env, "util-stopped-a");
    live_send_to_terminal(&mut env, &id);
    env.view.rebuild_flat_items();
    assert!(
        env.view.live_send.is_some(),
        "shown under Attention, so still live"
    );

    env.view.apply_sort_order(SortOrder::AZ);

    assert!(!session_titles(&env.view).contains(&"util-stopped-a".to_string()));
    assert!(env.view.live_send.is_none());
}

/// Live-send on an ungrouped stopped session ends when a grouping change (as the group picker
/// submits it) puts the session in a group the filter hides it from.
#[test]
#[serial]
fn live_send_ends_when_a_grouping_change_hides_its_session() {
    let mut env = env_with_stopped(true);
    press_y(&mut env);
    let id = select_session(&mut env, "loose-stopped");
    live_send_to_terminal(&mut env, &id);
    env.view.rebuild_flat_items();
    assert!(
        env.view.live_send.is_some(),
        "ungrouped, so shown and still live"
    );

    env.view.apply_group_by(GroupByMode::Project);

    assert!(!session_titles(&env.view).contains(&"loose-stopped".to_string()));
    assert!(env.view.live_send.is_none());
}

fn cursor_row_session(view: &HomeView) -> Option<String> {
    match view.flat_items.get(view.cursor)? {
        Item::Session { id, .. } => Some(id.clone()),
        Item::Group { .. } => None,
    }
}

/// A sort or grouping change can start hiding the selected session. The selection moves to
/// that session's own group header, never to whichever session now sits at its old index.
#[test]
#[serial]
fn a_projection_change_that_hides_the_selection_selects_its_own_header() {
    type Change = fn(&mut HomeView);
    let cases: [(&str, &str, Change, Change); 2] = [
        (
            "sort",
            "util-stopped-a",
            |view| view.apply_sort_order(SortOrder::Attention),
            |view| view.apply_sort_order(SortOrder::AZ),
        ),
        (
            "grouping",
            "loose-stopped",
            |_| {},
            |view| view.apply_group_by(GroupByMode::Project),
        ),
    ];
    for (case, title, before, change) in cases {
        let mut env = env_with_stopped(true);
        before(&mut env.view);
        press_y(&mut env);
        let id = select_session(&mut env, title);
        assert_eq!(
            env.view.selected_session.as_deref(),
            Some(id.as_str()),
            "{case}"
        );

        change(&mut env.view);

        assert!(
            !session_titles(&env.view).contains(&title.to_string()),
            "{case}: hidden"
        );
        assert_eq!(
            env.view.selected_session, None,
            "{case}: no other session selected"
        );
        let header = match env.view.group_by {
            GroupByMode::Project => {
                super::super::rows::project_group_key(env.view.get_instance(&id).unwrap())
            }
            _ => env.view.get_instance(&id).unwrap().group_path.clone(),
        };
        assert!(
            matches!(&env.view.flat_items[env.view.cursor], Item::Group { path, .. } if *path == header),
            "{case}: cursor on its own {header} header"
        );
        assert_eq!(
            env.view.selected_group.as_deref(),
            Some(header.as_str()),
            "{case}"
        );
    }
}

/// `w` passes over an unread session the filter hides: selecting it would leave the preview and
/// the next action on a row the list does not show. Shown again, the same session is the target.
#[test]
#[serial]
fn w_skips_an_unread_session_the_filter_hides() {
    let unread_before = crate::session::unread_enabled();
    crate::session::set_unread_enabled(true);
    let mut env = env_with_stopped(true);
    let hidden = select_session(&mut env, "util-stopped-a");
    env.view.mutate_instance(&hidden, |inst| inst.mark_unread());
    press_y(&mut env);
    let live = select_session(&mut env, "util-live");

    {
        let result = env.view.handle_key(key(KeyCode::Char('w')), None);
        drain_persistence(&mut env.view).unwrap();
        result
    };
    assert_eq!(env.view.selected_session.as_deref(), Some(live.as_str()));
    assert_eq!(
        cursor_row_session(&env.view).as_deref(),
        Some(live.as_str())
    );

    env.view.info_dialog = None;
    press_y(&mut env);
    select_session(&mut env, "util-live");
    {
        let result = env.view.handle_key(key(KeyCode::Char('w')), None);
        drain_persistence(&mut env.view).unwrap();
        result
    };
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(hidden.as_str()),
        "shown, the unread session is what w finds"
    );
    assert_eq!(
        cursor_row_session(&env.view).as_deref(),
        Some(hidden.as_str())
    );
    crate::session::set_unread_enabled(unread_before);
}

/// Restoring a stopped session from Archived or Trash while the filter is on brings it back
/// into a group that hides it. The selection follows it to its group's header, so the cursor
/// row and the session the next key acts on agree.
#[test]
#[serial]
fn restoring_a_session_the_filter_hides_selects_its_own_header() {
    type Shelve = fn(&mut HomeView, &str);
    let cases: [(&str, Shelve); 2] = [
        ("archive", |view, id| {
            view.select_session_by_id(id);
            {
                let submitted = view.toggle_archive_at_cursor();
                await_transaction_result(
                    view,
                    submitted.map(|_| super::super::TransactionDisposition::Queued),
                )
            }
            .unwrap();
            finish_runner_settlements(view);
            assert!(view.get_instance(id).unwrap().is_archived());
        }),
        ("trash", |view, id| {
            view.selected_session = Some(id.to_string());
            {
                view.trash_session_by_id(id);
                drain_persistence(view).unwrap();
            };
            assert!(view.get_instance(id).unwrap().is_trashed());
        }),
    ];
    for (case, shelve) in cases {
        let mut env = env_with_stopped(true);
        env.view.archived_section_collapsed = false;
        env.view.trashed_section_collapsed = false;
        let id = select_session(&mut env, "util-stopped-a");
        shelve(&mut env.view, &id);
        press_y(&mut env);
        select_session(&mut env, "util-stopped-a");
        assert_eq!(
            env.view.selected_session.as_deref(),
            Some(id.as_str()),
            "{case}: on the shelf"
        );

        {
            let submitted = env.view.toggle_archive_at_cursor();
            await_transaction_result(
                &mut env.view,
                submitted.map(|_| super::super::TransactionDisposition::Queued),
            )
        }
        .unwrap();

        let inst = env.view.get_instance(&id).unwrap();
        assert!(
            !inst.is_archived() && !inst.is_trashed(),
            "{case}: restored"
        );
        assert_eq!(
            inst.status,
            Status::Stopped,
            "{case}: restore keeps it stopped"
        );
        assert!(
            !session_titles(&env.view).contains(&"util-stopped-a".to_string()),
            "{case}: hidden"
        );
        assert_eq!(
            env.view.selected_session, None,
            "{case}: the hidden session is not the target"
        );
        assert!(
            matches!(&env.view.flat_items[env.view.cursor], Item::Group { path, .. } if path == "util"),
            "{case}: cursor on its util header"
        );
        assert_eq!(env.view.selected_group.as_deref(), Some("util"), "{case}");
    }
}

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
    env.view.handle_key(key(KeyCode::Char('y')), None);
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
    env.view.handle_key(key(KeyCode::Char('Y')), None);
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

    env.view.move_row_at_cursor(1).unwrap();
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
    env.view.move_row_at_cursor(1).unwrap();
    assert!(
        !env.view
            .status_flash_text()
            .is_some_and(|t| t.contains("Show stopped sessions")),
        "a group header moves among groups, which hiding does not touch"
    );

    press_y(&mut env);
    select_session(&mut env, "util-live");
    env.view.move_row_at_cursor(-1).unwrap();
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
    view.handle_key(key(KeyCode::Char('y')), None);

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
    env.view.reload_storage_only().unwrap();

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

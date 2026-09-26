//! Left-click on a session row selects it, like arrow-key navigation. Clicks outside the
//! inner list rect, on a row past the last item, or while a dialog is open are no-ops.

use super::*;
use ratatui::layout::Rect;

/// Inner rect chosen with comfortable headroom so all sessions fit
/// without "[N more above/below]" indicators consuming a row.
fn setup_inner(env: &mut TestEnv) {
    env.view.list_inner_area = Rect::new(1, 1, 28, 10);
}

#[test]
#[serial]
fn click_selects_session_at_clicked_row() {
    // A single click selects the row and requests live mode. Re-clicking the selected row
    // past the double-click threshold is a fresh single click: it re-requests live mode and
    // never attaches.
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();
    let expected = Some(crate::tui::app::Action::EnterLiveSend(
        session_id_at(&env.view, 2).unwrap(),
    ));

    let t0 = std::time::Instant::now();
    assert_eq!(env.view.handle_click_at(t0, 5, 3), expected);
    assert_eq!(env.view.cursor, 2);
    let t1 = t0 + std::time::Duration::from_millis(1500);
    assert_eq!(env.view.handle_click_at(t1, 5, 3), expected);
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn select_only_click_on_different_row_exits_live_mode() {
    // With SelectOnly, clicking a different row while live-sending must leave live mode, or
    // keystrokes stay aimed at the old session while the cursor and preview walk away. The
    // click still emits no action and still moves the cursor.
    use crate::session::config::{update_config, ClickAction};
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::SelectOnly;
    })
    .unwrap();

    let live_id = env.view.selected_session.clone().unwrap();
    env.view.live_send = Some(LiveSendState {
        session_id: live_id,
        title: "live".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    let action = env.view.handle_click(5, 3);
    assert_eq!(action, None, "SelectOnly click never emits an action");
    assert_eq!(env.view.cursor, 2, "the click still moves the cursor");
    assert!(
        env.view.live_send.is_none(),
        "clicking a different row in SelectOnly mode must exit live mode"
    );
}

#[test]
#[serial]
fn select_only_click_on_live_row_exits_live_mode() {
    // Clicking the row that is already live-sending is a "leave" gesture: in SelectOnly a
    // single click selects it and drops out of live mode, rather than stranding keystrokes
    // the user is stepping away from.
    use crate::session::config::{update_config, ClickAction};
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    // Row 3 resolves to index 2, so make index 2 the live row.
    env.view.cursor = 2;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::SelectOnly;
    })
    .unwrap();

    let live_id = env.view.selected_session.clone().unwrap();
    env.view.live_send = Some(LiveSendState {
        session_id: live_id,
        title: "live".to_string(),
        tmux_name: "aoe_test_live".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    let action = env.view.handle_click(5, 3);
    assert_eq!(action, None, "SelectOnly click never emits an action");
    assert!(
        env.view.live_send.is_none(),
        "clicking the already-live row must exit live mode"
    );
}

#[test]
#[serial]
fn single_click_on_archived_row_selects_without_reviving() {
    // A parked (archived) session has had its pane killed, so a single click is a "let me
    // look" gesture: no EnterLiveSend to respawn the pane and no auto-unarchive, even under
    // the default `click_action = LiveSend`. Bringing it back stays explicit.
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    // Keep archived rows visible so the archived row is clickable.
    env.view.archived_section_collapsed = false;

    // Archive the row at cursor 0.
    env.view.cursor = 0;
    env.view.update_selected();
    let archived_id = env.view.selected_session.clone().unwrap();
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(
        env.view.get_instance(&archived_id).unwrap().is_archived(),
        "precondition: the session must be archived"
    );

    // Locate the archived row in the flat list and click it.
    let idx = env
        .view
        .flat_items
        .iter()
        .position(|it| matches!(it, Item::Session { id, .. } if id == &archived_id))
        .expect("archived session must render under the expanded Archived section");
    // Archived rows live in the pinned shelf, so render a real frame to populate
    // `shelf_inner_area` and click the shelf row rather than the faked list rect.
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, idx);
    let action = env.view.handle_click(5, row);

    assert_eq!(
        action, None,
        "single click on an archived row must not request live-send"
    );
    assert!(
        env.view.live_send.is_none(),
        "single click on an archived row must not enter live-send mode"
    );
    assert!(
        env.view.get_instance(&archived_id).unwrap().is_archived(),
        "single click on an archived row must not unarchive it"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(archived_id.as_str()),
        "single click should still select the archived row"
    );
}

#[test]
#[serial]
fn select_only_click_honors_per_profile_override() {
    // Global stays LiveSend while the test profile pins SelectOnly through
    // SessionConfigOverride, so the resolver must pick the override: a single click returns
    // None and the cursor still moves.
    use crate::session::config::profile_config::{save_profile_config, ProfileConfig};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let profile_config: ProfileConfig =
        serde_json::from_value(serde_json::json!({"session": {"click_action": "select_only"}}))
            .unwrap();
    save_profile_config("test", &profile_config).unwrap();

    let action = env.view.handle_click(5, 3);
    assert_eq!(
        action, None,
        "per-profile SelectOnly must override the LiveSend global default"
    );
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn double_click_still_attaches_under_select_only() {
    // Defensive: `SelectOnly` changes only single-click, so double-click must still
    // activate through `default_attach_mode` (Tmux by default), locking the separation
    // between the two settings.
    use crate::session::config::{update_config, ClickAction};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::SelectOnly;
    })
    .unwrap();

    let t0 = std::time::Instant::now();
    let first = env.view.handle_click_at(t0, 5, 3);
    assert_eq!(
        first, None,
        "first click under SelectOnly must not emit an action"
    );
    let t1 = t0 + std::time::Duration::from_millis(100);
    let second = env.view.handle_click_at(t1, 5, 3);
    let expected_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    assert_eq!(
        second,
        Some(crate::tui::app::Action::AttachSession(expected_id)),
        "double-click must still activate via default_attach_mode (Tmux)"
    );
}

#[test]
#[serial]
fn double_click_tears_down_live_send_before_tmux_attach() {
    // #2290: with `click_action = LiveSend` the first click of a double enters live-send
    // and the second resolves to a tmux attach, which must exit live mode first or the
    // worker is stranded against a pane we are leaving and detaching returns to live mode.
    use crate::session::config::{update_config, ClickAction};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::LiveSend;
    })
    .unwrap();

    // Simulate the first click having already entered live-send (the real install runs in
    // App::execute_action, which a HomeView unit test can't drive).
    let expected_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
        session_id: expected_id.clone(),
        title: "row-2".to_string(),
        tmux_name: "aoe_test_2290".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    let t0 = std::time::Instant::now();
    // Seed last_click so the next click within the threshold is treated
    // as the second click of a double-click on the same row.
    env.view.last_click = Some((t0, 5, 3));
    let t1 = t0 + std::time::Duration::from_millis(100);
    let action = env.view.handle_click_at(t1, 5, 3);

    assert_eq!(
        action,
        Some(crate::tui::app::Action::AttachSession(expected_id)),
        "double-click must still resolve to a tmux attach under default_attach_mode (Tmux)"
    );
    assert!(
        env.view.live_send.is_none(),
        "the tmux attach path must exit live mode first, not strand the worker"
    );
}

#[test]
#[serial]
fn clicks_off_session_rows_or_under_a_dialog_are_noops() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // inner = (1, 1, 28, 10) holding three rows: row 5 is inside but past the last item,
    // row 0 is above the rect, and column 50 is past its right edge.
    for (col, row) in [(5, 5), (5, 0), (50, 2)] {
        assert!(env.view.handle_click(col, row).is_none(), "({col}, {row})");
        assert_eq!(env.view.cursor, 0);
    }

    env.view.show_help = true;
    assert!(
        env.view.handle_click(5, 3).is_none(),
        "dialog should swallow the click"
    );
    assert_eq!(env.view.cursor, 0);
}

/// A double-click on the preview produces the same activation Action a sidebar double-click
/// would: the first press records timing, the second within the threshold attaches the
/// previewed session.
#[test]
#[serial]
fn preview_double_click_attaches_like_sidebar() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    env.view.preview_area = Rect::new(30, 0, 100, 40);
    env.view.cursor = 1;
    env.view.update_selected();
    let expected_id = env
        .view
        .selected_session
        .clone()
        .expect("a session is selected");

    // (50, 10) is inside preview_area (30, 0, 100, 40).
    let t0 = Instant::now();
    assert_eq!(
        env.view.preview_double_click_action_at(
            t0,
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ),
        None,
        "a single preview press does not activate"
    );
    let t1 = t0 + Duration::from_millis(150);
    assert_eq!(
        env.view.preview_double_click_action_at(
            t1,
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            50,
            10
        ),
        Some(crate::tui::app::Action::AttachSession(expected_id)),
        "a double-click on the preview attaches the session, same as the sidebar"
    );
}

/// Shift+press (aoe's own selection escape hatch), presses outside the preview, and a
/// second press on a different cell never activate, even within the double-click window.
#[test]
#[serial]
fn preview_shift_and_off_pane_presses_never_activate() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    env.view.preview_area = Rect::new(30, 0, 100, 40);
    env.view.cursor = 1;
    env.view.update_selected();

    let t0 = Instant::now();
    let t1 = t0 + Duration::from_millis(150);
    let down = MouseEventKind::Down(MouseButton::Left);
    // Shift falls through to aoe selection: never tracked, never activates.
    assert_eq!(
        env.view
            .preview_double_click_action_at(t0, down, KeyModifiers::SHIFT, 50, 10),
        None
    );
    assert_eq!(
        env.view
            .preview_double_click_action_at(t1, down, KeyModifiers::SHIFT, 50, 10),
        None
    );
    // A press in the list area (5, 3), outside the preview, is ignored too.
    assert_eq!(
        env.view
            .preview_double_click_action_at(t0, down, KeyModifiers::NONE, 5, 3),
        None
    );
    assert_eq!(
        env.view
            .preview_double_click_action_at(t1, down, KeyModifiers::NONE, 5, 3),
        None
    );
    // Same row 10, columns 40 then 70: within the time window but a different cell, so
    // the second press is a fresh single click.
    let t2 = t1 + Duration::from_secs(5);
    assert_eq!(
        env.view
            .preview_double_click_action_at(t2, down, KeyModifiers::NONE, 40, 10),
        None
    );
    assert_eq!(
        env.view.preview_double_click_action_at(
            t2 + Duration::from_millis(150),
            down,
            KeyModifiers::NONE,
            70,
            10
        ),
        None,
        "a different-column second press on the same row must not activate"
    );
}

#[test]
#[serial]
fn two_clicks_on_different_rows_do_not_activate() {
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let id_row2 = match &env.view.flat_items[1] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[1] should be a session"),
    };
    let id_row3 = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    let t0 = Instant::now();
    let first = env.view.handle_click_at(t0, 5, 2);
    assert_eq!(
        first,
        Some(crate::tui::app::Action::EnterLiveSend(id_row2)),
        "first click enters live mode for its row"
    );
    let t1 = t0 + Duration::from_millis(100);
    let second = env.view.handle_click_at(t1, 5, 3);
    assert_eq!(
        second,
        Some(crate::tui::app::Action::EnterLiveSend(id_row3)),
        "different-row second click is a fresh single click that switches the live target, not a double-click attach"
    );
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn double_click_activates_clicked_row_even_if_cursor_moved_between_clicks() {
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // Capture the id at flat_items[2] so we know which session
    // the row-3 click is targeting.
    let clicked_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    let t0 = Instant::now();
    let first = env.view.handle_click_at(t0, 5, 3);
    assert_eq!(
        first,
        Some(crate::tui::app::Action::EnterLiveSend(clicked_id.clone()))
    );
    assert_eq!(env.view.cursor, 2);

    // Simulate the cursor drifting away between clicks, as an arrow press or an async list
    // refresh would.
    env.view.cursor = 0;
    env.view.update_selected();

    let t1 = t0 + Duration::from_millis(150);
    let action = env.view.handle_click_at(t1, 5, 3);
    assert_eq!(
        action,
        Some(crate::tui::app::Action::AttachSession(clicked_id)),
        "double-click must activate the row that was clicked, \
         not whatever the cursor drifted to"
    );
    assert_eq!(
        env.view.cursor, 2,
        "double-click should also re-sync cursor onto the clicked row"
    );
}

/// Already live on session A, clicking a different row emits `EnterLiveSend(B)` so the
/// caller can switch the live target.
#[test]
#[serial]
fn click_on_other_session_while_live_switches_target() {
    use crate::tui::home::live_send::LiveSendState;

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let id_a = match &env.view.flat_items[1] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[1] should be a session"),
    };
    let id_b = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    // Simulate already being in live mode for session A.
    env.view.live_send = Some(LiveSendState {
        session_id: id_a.clone(),
        title: "session1".to_string(),
        tmux_name: format!("aoe_test_{}", id_a),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    // Click session B's row.
    let action = env.view.handle_click(5, 3);
    assert_eq!(
        action,
        Some(crate::tui::app::Action::EnterLiveSend(id_b)),
        "clicking a different session row while live must switch the live target"
    );
}

/// Clicking the row that is already the live-send target is a no-op: re-running
/// `prepare_live_send` would drop the worker and redo ensure_pane_ready for nothing.
#[test]
#[serial]
fn click_on_already_live_session_is_noop() {
    use crate::tui::home::live_send::LiveSendState;

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let id_a = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    env.view.live_send = Some(LiveSendState {
        session_id: id_a.clone(),
        title: "session2".to_string(),
        tmux_name: format!("aoe_test_{}", id_a),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    let action = env.view.handle_click(5, 3);
    assert!(
        action.is_none(),
        "clicking the already-live session row should not re-enter live mode"
    );
    assert_eq!(env.view.cursor, 2, "selection still updates");
}

/// Creating and structured (ACP) sessions can't host live mode, so a single click only
/// selects the row; a Creating row is not attachable by double-click either.
#[test]
#[serial]
fn click_on_session_that_cannot_go_live_only_selects() {
    use std::time::{Duration, Instant};

    let cases: [(&str, fn(&mut Instance)); 2] = [
        ("creating", |inst| {
            inst.status = crate::session::Status::Creating
        }),
        ("structured", |inst| {
            inst.view = crate::session::View::Structured
        }),
    ];
    for (label, mutate) in cases {
        let mut env = create_test_env_with_sessions(3);
        setup_inner(&mut env);
        env.view.cursor = 0;
        env.view.update_selected();
        let target_id = session_id_at(&env.view, 2).unwrap();
        env.view.mutate_instance(&target_id, mutate);

        let t0 = Instant::now();
        assert!(
            env.view.handle_click_at(t0, 5, 3).is_none(),
            "{label}: click is a selection only"
        );
        assert_eq!(env.view.cursor, 2, "{label}");
        if label == "creating" {
            let t1 = t0 + Duration::from_millis(150);
            assert!(
                env.view.handle_click_at(t1, 5, 3).is_none(),
                "Creating sessions are not attachable; double-click should noop"
            );
        }
    }
}

#[test]
#[serial]
fn hover_tracks_the_row_under_the_mouse() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    assert!(
        env.view.handle_hover(5, 3),
        "first hover over a fresh row should request redraw"
    );
    assert_eq!(env.view.hovered_index(), Some(2));
    assert!(env.view.handle_hover(5, 2), "a new row requests redraw");
    assert_eq!(env.view.hovered_index(), Some(1));
    assert!(
        !env.view.handle_hover(6, 2),
        "same-row movement should not trigger a redraw request"
    );
    assert_eq!(env.view.hovered_index(), Some(1));
    // Row 0 is above the inner rect (inner.y = 1).
    assert!(
        env.view.handle_hover(5, 0),
        "leaving the list should request a redraw"
    );
    assert_eq!(env.view.hovered_index(), None);
    env.view.handle_hover(5, 5);
    assert_eq!(env.view.hovered_index(), None, "below the last item");

    // Keyboard nav must clear hover: when a prediction layer eats the off-list `Moved`
    // event, a stale `mouse_pos` would paint two rows at once.
    env.view.handle_hover(5, 2);
    env.view.move_cursor(1);
    assert_eq!(env.view.hovered_index(), None);

    env.view.show_help = true;
    env.view.handle_hover(5, 2);
    assert_eq!(env.view.hovered_index(), None, "a dialog swallows hover");
}

#[test]
#[serial]
fn changing_session_clears_preview_selection() {
    // A finalized preview selection pins to the previous pane's cells and freezes the
    // preview while held, so carrying it into another session would paint a stale highlight
    // and stop the new session's output from following.
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    let sessions: Vec<usize> = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it, Item::Session { .. }))
        .map(|(i, _)| i)
        .collect();
    assert!(sessions.len() >= 2, "test needs two session rows");

    env.view.cursor = sessions[0];
    env.view.update_selected();
    let first = env.view.selected_session.clone();
    assert!(first.is_some(), "precondition: a session is selected");
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, 0),
        extent: (4, 2),
        finalized: true,
    });

    env.view.cursor = sessions[1];
    env.view.update_selected();
    assert_ne!(
        env.view.selected_session, first,
        "precondition: cursor moved to a different session"
    );
    assert!(
        env.view.preview_selection.is_none(),
        "changing sessions must clear the stale selection so the new preview isn't frozen"
    );
}

#[test]
#[serial]
fn click_on_group_row_toggles_collapsed() {
    let mut env = create_test_env_with_mixed_sessions();
    setup_inner(&mut env);

    // Find the first group row in flat_items; record initial collapsed.
    let (group_idx, group_path) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(i, item)| match item {
            crate::session::Item::Group { path, .. } => Some((i, path.clone())),
            _ => None,
        })
        .expect("mixed env should produce at least one group row");

    let click_row = env.view.list_inner_area.y + group_idx as u16;
    let was_collapsed = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Group {
                path, collapsed, ..
            } if path == &group_path => Some(*collapsed),
            _ => None,
        })
        .unwrap();

    let action = env.view.handle_click(5, click_row);
    assert!(
        action.is_none(),
        "single click on a group should not activate"
    );

    let now_collapsed = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            crate::session::Item::Group {
                path, collapsed, ..
            } if path == &group_path => Some(*collapsed),
            _ => None,
        })
        .expect("group row should still be present after toggle");
    assert_ne!(was_collapsed, now_collapsed, "group collapsed state flips");
}

//! Left-click on a session row in the list selects it (same effect as
//! arrow-key navigation). Clicks outside the inner list rect, clicks on
//! a row past the last item, and clicks while a dialog is open are
//! no-ops.

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
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // Click the third visible row (inner.y + 2 == 3) -> flat_items[2].
    // Single-click on a session row both selects it AND requests
    // live-send mode for that row.
    let action = env.view.handle_click(5, 3);
    let expected_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    assert_eq!(
        action,
        Some(crate::tui::app::Action::EnterLiveSend(expected_id)),
        "single click should select the row and request live mode"
    );
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn select_only_click_moves_cursor_without_entering_live_mode() {
    // With `click_action = SelectOnly`, a single click must move the
    // cursor (so the preview pane updates) but NOT emit
    // EnterLiveSend. Double-click + Enter still activate the row,
    // but that path is gated by `default_attach_mode`, not this
    // setting, so it's exercised elsewhere.
    use crate::session::config::{update_config, ClickAction};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::SelectOnly;
    })
    .unwrap();

    let action = env.view.handle_click(5, 3);
    assert_eq!(
        action, None,
        "SelectOnly must not emit EnterLiveSend on single click"
    );
    assert_eq!(
        env.view.cursor, 2,
        "SelectOnly must still move the cursor to the clicked row"
    );
}

#[test]
#[serial]
fn select_only_click_on_different_row_exits_live_mode() {
    // With `click_action = SelectOnly`, clicking a *different* row while
    // live-sending must leave live mode (otherwise keystrokes stay aimed at
    // the old session while the cursor/preview walk away). The click still
    // emits no action and still moves the cursor.
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
    // Clicking the row that's already live-sending is a "leave" gesture:
    // in SelectOnly mode a single click on the active session selects it and
    // drops out of live mode, rather than doing nothing (the cursor is
    // already there, but staying live strands keystrokes the user is trying
    // to step away from).
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
    // A parked (archived) session has had its pane killed. Single-clicking
    // it is a "let me look" gesture and must NOT resurrect it: no
    // EnterLiveSend (which would respawn the pane) and the session stays
    // archived (no auto-unarchive). This holds even under the default
    // `click_action = LiveSend`. Bringing it back stays explicit: `z`,
    // double-click, or Enter.
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
    // Archived rows now live in the pinned shelf, so render a real frame to
    // populate `shelf_inner_area` and click the shelf row, not the faked
    // list rect.
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
    // Global stays LiveSend (default) but the test profile pins
    // SelectOnly via SessionConfigOverride. The resolver must
    // pick the profile override, not the global default, so a
    // single click returns None and the cursor still moves.
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
    // Defensive: `SelectOnly` only changes single-click; double-click
    // must still activate the row via `default_attach_mode` (Tmux by
    // default, so we expect AttachSession). Locks down the
    // separation between the two settings so a future refactor
    // can't accidentally route double-click through `click_action`.
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
    // Regression for #2290: with `click_action = LiveSend` the first
    // click of a double-click enters live-send for the row, then the
    // second click resolves to a tmux attach via
    // `default_attach_mode = Tmux`. The attach must exit live mode
    // first, otherwise the worker is stranded against a pane we're
    // leaving and detaching drops the user back into live mode.
    use crate::session::config::{update_config, ClickAction};
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    update_config(|config| {
        config.session.click_action = ClickAction::LiveSend;
    })
    .unwrap();

    // Simulate the first click of the double having already entered
    // live-send (the real install runs in App::execute_action, which a
    // HomeView unit test can't drive): pin live_send to the row we are
    // about to double-click.
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
fn click_on_already_selected_row_does_not_move_cursor() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();

    // Re-clicking the already-selected row still requests live mode
    // (the row is now eligible to be the live target); cursor stays
    // put.
    let action = env.view.handle_click(5, 2);
    let expected_id = match &env.view.flat_items[1] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[1] should be a session"),
    };
    assert_eq!(
        action,
        Some(crate::tui::app::Action::EnterLiveSend(expected_id))
    );
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn click_below_last_item_is_noop() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // inner.y=1, three items occupy rows 1..=3. Row 5 is inside the
    // inner rect but past the last item.
    let action = env.view.handle_click(5, 5);
    assert!(action.is_none());
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn click_outside_inner_rect_is_noop() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // Row 0 is above inner.y; column 50 is past inner.x + inner.width.
    assert!(env.view.handle_click(5, 0).is_none());
    assert!(env.view.handle_click(50, 2).is_none());
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn click_with_dialog_open_is_noop() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.show_help = true;

    let action = env.view.handle_click(5, 3);
    assert!(action.is_none(), "dialog should swallow the click");
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn double_click_on_session_returns_attach_action() {
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();
    let expected_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    let t0 = Instant::now();
    let first = env.view.handle_click_at(t0, 5, 3);
    assert_eq!(
        first,
        Some(crate::tui::app::Action::EnterLiveSend(expected_id.clone())),
        "first click selects and requests live mode"
    );
    assert_eq!(env.view.cursor, 2);

    let t1 = t0 + Duration::from_millis(150);
    let second = env.view.handle_click_at(t1, 5, 3);
    assert_eq!(
        second,
        Some(crate::tui::app::Action::AttachSession(expected_id)),
        "second click within threshold should attach the session"
    );
}

/// A double-click on the preview pane produces the SAME activation Action a
/// sidebar double-click would (parity gesture): the first press is a no-op
/// that records timing, the second within threshold attaches the previewed
/// session.
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

/// Shift+press (aoe's own selection escape hatch) and presses outside the
/// preview never activate, even repeated within the double-click window.
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
}

/// Two presses on the same preview row but different columns are unrelated
/// clicks (e.g. tapping two different words), not a double-click: only a
/// same-cell second press within the window activates.
#[test]
#[serial]
fn preview_two_presses_on_same_row_different_col_do_not_activate() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    env.view.preview_area = Rect::new(30, 0, 100, 40);
    env.view.cursor = 1;
    env.view.update_selected();

    let down = MouseEventKind::Down(MouseButton::Left);
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_millis(150);
    // Same row 10, columns 40 then 70: within the time window but a
    // different cell, so the second press is a fresh single click.
    assert_eq!(
        env.view
            .preview_double_click_action_at(t0, down, KeyModifiers::NONE, 40, 10),
        None
    );
    assert_eq!(
        env.view
            .preview_double_click_action_at(t1, down, KeyModifiers::NONE, 70, 10),
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
fn click_after_threshold_does_not_activate() {
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();
    let id_row3 = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    let t0 = Instant::now();
    env.view.handle_click_at(t0, 5, 3);
    let t1 = t0 + Duration::from_millis(1500);
    let action = env.view.handle_click_at(t1, 5, 3);
    // Past the double-click threshold the second click is a fresh
    // single click that re-requests live mode for the row; it
    // never attaches.
    assert_eq!(
        action,
        Some(crate::tui::app::Action::EnterLiveSend(id_row3))
    );
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

    // Simulate the cursor drifting away between clicks (e.g., a
    // keyboard arrow press or an async list refresh that selected
    // a different row).
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

#[test]
#[serial]
fn double_click_on_creating_session_returns_no_action() {
    use std::time::{Duration, Instant};

    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    // Force the target session into Creating; activation must bail.
    let target_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    env.view.mutate_instance(&target_id, |inst| {
        inst.status = crate::session::Status::Creating;
    });

    let t0 = Instant::now();
    env.view.handle_click_at(t0, 5, 3);
    let t1 = t0 + Duration::from_millis(150);
    let action = env.view.handle_click_at(t1, 5, 3);
    assert!(
        action.is_none(),
        "Creating sessions are not attachable; double-click should noop"
    );
}

/// Single click on a session row enters live-send mode for that
/// session (the same `Action::EnterLiveSend` that Tab emits) in
/// addition to selecting the row.
#[test]
#[serial]
fn single_click_on_session_emits_enter_live_send() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let target_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };

    let action = env.view.handle_click(5, 3);
    assert_eq!(
        action,
        Some(crate::tui::app::Action::EnterLiveSend(target_id))
    );
    assert_eq!(env.view.cursor, 2);
}

/// Already in live mode for session A; clicking a different
/// session row emits `EnterLiveSend(B)` so the caller can switch
/// the live target.
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

/// Clicking the row that is already the live-send target is a
/// no-op: re-running `prepare_live_send` would drop the worker and
/// re-do ensure_pane_ready for no reason.
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

/// Creating/Deleting sessions can't host live mode, so a single
/// click selects the row but emits no action.
#[test]
#[serial]
fn single_click_on_creating_session_returns_no_action() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let target_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    env.view.mutate_instance(&target_id, |inst| {
        inst.status = crate::session::Status::Creating;
    });

    let action = env.view.handle_click(5, 3);
    assert!(
        action.is_none(),
        "Creating sessions can't enter live mode; click is a selection only"
    );
    assert_eq!(env.view.cursor, 2);
}

/// Acp-mode sessions are not tmux-backed, so click cannot
/// enter live mode for them; selection still updates.
#[test]
#[serial]
fn single_click_on_acp_session_returns_no_action() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let target_id = match &env.view.flat_items[2] {
        crate::session::Item::Session { id, .. } => id.clone(),
        _ => panic!("flat_items[2] should be a session"),
    };
    env.view.mutate_instance(&target_id, |inst| {
        inst.view = crate::session::View::Structured;
    });

    let action = env.view.handle_click(5, 3);
    assert!(
        action.is_none(),
        "Acp sessions can't enter live mode; click is a selection only"
    );
    assert_eq!(env.view.cursor, 2);
}

#[test]
#[serial]
fn hover_sets_resolved_index_for_row_under_mouse() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    let changed = env.view.handle_hover(5, 3);
    assert!(
        changed,
        "first hover over a fresh row should request redraw"
    );
    assert_eq!(env.view.hovered_index(), Some(2));
}

#[test]
#[serial]
fn hover_moving_to_a_new_row_requests_redraw() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.handle_hover(5, 1);
    let changed = env.view.handle_hover(5, 2);
    assert!(changed);
    assert_eq!(env.view.hovered_index(), Some(1));
}

#[test]
#[serial]
fn hover_pixel_twitch_on_same_row_is_noop() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.handle_hover(5, 2);
    let changed = env.view.handle_hover(6, 2);
    assert!(
        !changed,
        "same-row movement should not trigger a redraw request"
    );
    assert_eq!(env.view.hovered_index(), Some(1));
}

#[test]
#[serial]
fn hover_leaving_list_clears_resolved_index() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.handle_hover(5, 2);
    assert_eq!(env.view.hovered_index(), Some(1));

    // Row 0 is above the inner rect (inner.y = 1).
    let changed = env.view.handle_hover(5, 0);
    assert!(changed, "leaving the list should request a redraw");
    assert_eq!(env.view.hovered_index(), None);
}

#[test]
#[serial]
fn hover_resolves_to_none_when_dialog_open() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.show_help = true;
    env.view.handle_hover(5, 2);
    assert_eq!(env.view.hovered_index(), None);
}

#[test]
#[serial]
fn move_cursor_clears_hover() {
    // Repro for the keyboard-after-hover stuck-highlight bug: when
    // mosh (or any prediction layer) eats the off-list `Moved` event,
    // `mouse_pos` stays stuck on the row the mouse last touched while
    // the keyboard moves to a new row, painting two rows at once.
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.handle_hover(5, 2);
    assert_eq!(env.view.hovered_index(), Some(1));

    env.view.move_cursor(1);
    assert_eq!(
        env.view.hovered_index(),
        None,
        "keyboard nav must clear hover so only the selected row paints"
    );
}

#[test]
#[serial]
fn changing_session_clears_preview_selection() {
    // A finalized preview selection pins to the previous pane's cells, and
    // the preview freezes while a selection is held. Carried into a
    // different session it would both paint a stale highlight and stop the
    // new session's preview from following output, so selecting another
    // session must drop it.
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
fn hover_below_last_item_resolves_to_none() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);

    env.view.handle_hover(5, 5);
    assert_eq!(env.view.hovered_index(), None);
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

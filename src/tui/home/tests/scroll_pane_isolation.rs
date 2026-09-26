//! Wheel events stay confined to the pane the mouse is over: a wheel over the preview
//! never moves the list cursor, at a scroll boundary or with nothing selected (#1361).

use super::*;
use ratatui::layout::Rect;

fn setup_panes(env: &mut TestEnv) {
    env.view.list_area = Rect::new(0, 0, 30, 40);
    env.view.preview_area = Rect::new(30, 0, 100, 40);
}

/// A live-send env whose preview-capture worker reports the given cursor, so the
/// alternate-screen wheel-forwarding branch runs without a real full-screen pane.
fn live_env_with_cursor(cursor: crate::tmux::PaneCursor) -> TestEnv {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget, LiveSendWorker};
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view.live_send = Some(LiveSendState {
        session_id: "fake".to_string(),
        title: "fake".to_string(),
        tmux_name: "fake".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });
    env.view.live_send_worker = Some(LiveSendWorker::spawn("fake".to_string(), None));
    env.view
        .sync_preview_capture_worker(Some("fake".to_string()));
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 200;
    env.view.preview_scroll_offset = 10;
    env.view.preview_cache.cursor = Some(cursor);
    env.view.preview_cache.capture_target = Some("fake".to_string());
    env.view.preview_cache.capture_generation = env
        .view
        .preview_capture_worker
        .as_ref()
        .expect("capture worker")
        .current_generation_for_test();
    env
}

/// Like `live_env_with_cursor` but without entering live-send: the session is merely
/// previewed, with the capture worker and target set so `forward_wheel_to_preview` takes
/// the passive one-shot path.
fn passive_env_with_cursor(cursor: crate::tmux::PaneCursor) -> TestEnv {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view
        .sync_preview_capture_worker(Some("fake".to_string()));
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 200;
    env.view.preview_scroll_offset = 10;
    env.view.preview_capture_target = Some("fake".to_string());
    env.view.preview_cache.cursor = Some(cursor);
    env.view.preview_cache.capture_target = Some("fake".to_string());
    env.view.preview_cache.capture_generation = env
        .view
        .preview_capture_worker
        .as_ref()
        .expect("capture worker")
        .current_generation_for_test();
    env
}

fn alt_screen_cursor(
    alternate_on: bool,
    mouse_tracking: bool,
    mouse_sgr: bool,
) -> crate::tmux::PaneCursor {
    crate::tmux::PaneCursor {
        x: 0,
        y: 0,
        visible: true,
        pane_height: 24,
        history_size: 1800,
        pane_width: 80,
        alternate_on,
        mouse_tracking,
        mouse_sgr,
        mouse_all: false,
        position_reliable: true,
        composite_pane0: None,
    }
}

/// A full-screen live-send target with SGR mouse tracking gets the wheel forwarded to it
/// (returning to the live edge) instead of growing the useless normal-buffer capture
/// window, which is what made scrolling snap to the start of the session.
#[test]
#[serial]
fn wheel_over_alt_screen_sgr_mouse_pane_forwards_instead_of_scrollback() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));

    let up = env.view.handle_scroll_up(50, 10);
    assert!(up, "wheel over a full-screen SGR-mouse pane is handled");
    assert_eq!(
        env.view.preview_scroll_offset, 0,
        "forwarding pins the preview to the live edge, never the normal-buffer history"
    );

    env.view.preview_scroll_offset = 10;
    let down = env.view.handle_scroll_down(50, 10);
    assert!(down);
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// A full-screen app without mouse tracking reads arrows as cursor navigation rather than
/// scroll, so `PageUp`/`PageDown` are forwarded and the preview pins to the live edge, like
/// the mouse-tracking case. Regression for #2407.
#[test]
#[serial]
fn wheel_over_alt_screen_without_mouse_forwards_page_keys() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));

    let up = env.view.handle_scroll_up(50, 10);
    assert!(up, "wheel over a full-screen no-mouse pane is handled");
    assert_eq!(
        env.view.preview_scroll_offset, 0,
        "arrow-key forwarding pins the preview to the live edge, never the normal-buffer history"
    );

    env.view.preview_scroll_offset = 10;
    let down = env.view.handle_scroll_down(50, 10);
    assert!(down);
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// The wheel is forwarded (pinning the preview to the live edge) over any full-screen
/// mouse-tracking pane, passive or live, SGR or legacy X10. A normal-screen pane keeps the
/// capture-window scroll, since its scrollback is useful, and scrolling never exits live mode.
#[test]
#[serial]
fn wheel_over_preview_forwards_only_for_alternate_screen() {
    // (label, passive preview, cursor, forwards)
    let cases = [
        (
            "passive full-screen SGR",
            true,
            alt_screen_cursor(true, true, true),
            true,
        ),
        (
            "live full-screen legacy mouse",
            false,
            alt_screen_cursor(true, true, false),
            true,
        ),
        (
            "live normal screen",
            false,
            alt_screen_cursor(false, true, true),
            false,
        ),
    ];
    for (label, passive, cursor, forwards) in cases {
        let mut env = if passive {
            passive_env_with_cursor(cursor)
        } else {
            live_env_with_cursor(cursor)
        };
        assert_eq!(env.view.live_send.is_none(), passive, "{label}");
        assert!(env.view.handle_scroll_up(50, 10), "{label}");
        if forwards {
            assert_eq!(env.view.preview_scroll_offset, 0, "{label}: pinned");
        } else {
            assert!(
                env.view.preview_scroll_offset > 10,
                "{label}: capture scroll"
            );
            assert!(env.view.live_send.is_some(), "{label}: still live");
        }
    }
}

/// Over a mouse-tracking agent, live or passive, a press is forwarded and its button tracked
/// so the drag and release keep forwarding (live: even outside the preview rect) and the
/// release can't be stranded. Forwarding never starts an aoe text selection.
#[test]
#[serial]
fn forward_mouse_to_preview_tracks_press_through_release() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    // (label, passive preview, drag/release point)
    for (label, passive, (x, y)) in [("live", false, (1, 1)), ("passive", true, (55, 12))] {
        let cursor = alt_screen_cursor(true, true, true);
        let mut env = if passive {
            passive_env_with_cursor(cursor)
        } else {
            live_env_with_cursor(cursor)
        };
        let steps = [
            (MouseEventKind::Down(MouseButton::Left), (50, 10), Some(0)),
            (MouseEventKind::Drag(MouseButton::Left), (x, y), Some(0)),
            (MouseEventKind::Up(MouseButton::Left), (x, y), None),
        ];
        for (kind, (col, row), held) in steps {
            assert!(
                env.view
                    .forward_mouse_to_preview(kind, KeyModifiers::NONE, col, row),
                "{label}: {kind:?}"
            );
            assert_eq!(env.view.mouse_forward_btn, held, "{label}: {kind:?}");
        }
        assert!(env.view.drag_state.is_none(), "{label}");
        assert!(env.view.preview_selection.is_none(), "{label}");
    }
}

/// Events fall through to aoe when there is no forwardable press: Shift+press (so aoe's own
/// drag-to-copy runs), any press to a non-mouse agent, and a drag or release with no
/// forwarded press in flight (forwarding must not start mid-gesture).
#[test]
#[serial]
fn forward_mouse_to_preview_falls_through_without_a_forwardable_press() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let cases = [
        (
            "shift press",
            alt_screen_cursor(true, true, true),
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::SHIFT,
        ),
        (
            "non-mouse agent",
            alt_screen_cursor(true, false, false),
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
        ),
        (
            "orphan drag",
            alt_screen_cursor(true, true, true),
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::NONE,
        ),
        (
            "orphan release",
            alt_screen_cursor(true, true, true),
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::NONE,
        ),
    ];
    for (label, cursor, kind, modifiers) in cases {
        let mut env = live_env_with_cursor(cursor);
        assert!(
            !env.view.forward_mouse_to_preview(kind, modifiers, 50, 10),
            "{label}"
        );
        assert_eq!(env.view.mouse_forward_btn, None, "{label}");
    }
}

/// Bare motion is forwarded to an any-event-tracking (1003) agent so its hover UI works in
/// live mode, deduped per pane cell and re-armed when the pointer leaves and returns. A
/// button-tracking (1000/1002) or non-mouse agent never gets bare motion: it didn't ask for
/// it and would misparse the report.
#[test]
#[serial]
fn forward_hover_to_preview_reports_once_per_cell() {
    let mut cursor = alt_screen_cursor(true, true, true);
    cursor.mouse_all = true;
    let mut env = live_env_with_cursor(cursor);
    // The forward maps cells against the previewed pane's rect; give it
    // the preview area like a rendered frame would.
    env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);

    assert!(env.view.forward_hover_to_preview(50, 10));
    assert_eq!(env.view.hover_forward_cell, Some((21, 11)));
    // Same cell again: deduped, nothing sent.
    assert!(!env.view.forward_hover_to_preview(50, 10));
    // A different cell reports again.
    assert!(env.view.forward_hover_to_preview(51, 10));
    assert_eq!(env.view.hover_forward_cell, Some((22, 11)));
    // Leaving the preview clears the dedup cell (and forwards nothing)...
    assert!(!env.view.forward_hover_to_preview(1, 1));
    assert_eq!(env.view.hover_forward_cell, None);
    // ...so re-entering the same cell reports it to the agent again.
    assert!(env.view.forward_hover_to_preview(51, 10));

    for cursor in [
        alt_screen_cursor(true, true, true),
        alt_screen_cursor(true, false, false),
    ] {
        let mut env = live_env_with_cursor(cursor);
        env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);
        assert!(!env.view.forward_hover_to_preview(50, 10));
        assert_eq!(env.view.hover_forward_cell, None);
    }
}

/// Stage an in-flight Shift-selection drag held at the preview's top or bottom edge, plus a
/// capture window with no aoe-side scrollback, so `tick_preview_autoscroll` takes the agent
/// scroll-forward fallback rather than the capture-window line scroll.
fn stage_edge_drag_no_scrollback(env: &mut TestEnv, at_top: bool) {
    use crate::tui::home::PreviewTextView;
    // Visible == captured: `scroll_preview_offset` has nowhere to go, the alternate-screen
    // reality the fallback exists for. The clamp reads `preview_visible_rows`, so pin it to
    // the captured-line count to make the max offset zero.
    env.view.preview_cache.captured_lines = 23;
    env.view.preview_visible_rows = 23;
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_scroll_offset = 0;
    let pane = Rect::new(30, 0, 100, 5);
    env.view.preview_text_view = PreviewTextView {
        pane,
        first_line: 0,
        total_lines: 23,
    };
    // Anchor away from the held edge, then drag onto it.
    let (start_row, edge_row) = if at_top { (4, 0) } else { (0, 4) };
    assert!(env.view.handle_drag_start(40, start_row));
    assert!(env.view.handle_drag_move(40, edge_row));
}

/// Over a full-screen agent the capture window has no scrollback, so an edge-held selection
/// forwards the same input the wheel does (a mouse report to a mouse-tracking agent, page
/// keys otherwise) to scroll its own transcript. The byte output per branch is asserted in
/// `wheel_forward_key_*`. A normal-buffer pane that merely bottomed out its scrollback never
/// gets scroll input injected into its shell. The inert capture-window offset never moves.
#[test]
#[serial]
fn autoscroll_forwards_edge_drag_only_to_alternate_screen_agents() {
    // (label, cursor, held at top edge, forwards)
    let cases = [
        (
            "mouse agent, top edge",
            alt_screen_cursor(true, true, true),
            true,
            true,
        ),
        (
            "mouse agent, bottom edge",
            alt_screen_cursor(true, true, true),
            false,
            true,
        ),
        (
            "no-mouse agent, top edge",
            alt_screen_cursor(true, false, false),
            true,
            true,
        ),
        (
            "normal-buffer pane",
            alt_screen_cursor(false, false, false),
            true,
            false,
        ),
    ];
    for (label, cursor, at_top, forwards) in cases {
        let mut env = live_env_with_cursor(cursor);
        stage_edge_drag_no_scrollback(&mut env, at_top);
        assert_eq!(env.view.tick_preview_autoscroll(), forwards, "{label}");
        assert_eq!(env.view.preview_scroll_offset, 0, "{label}");
    }
}

/// Wheel-down over preview when offset is already at the bottom (0)
/// must NOT advance the list cursor.
#[test]
#[serial]
fn wheel_down_over_preview_at_bottom_does_not_move_list() {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.preview_scroll_offset = 0;

    let handled = env.view.handle_scroll_down(50, 10);

    assert!(
        !handled,
        "expected no-op when preview is at bottom boundary"
    );
    assert_eq!(env.view.cursor, 0, "list cursor must not move");
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// Wheel-up over preview when there is nothing more to scroll into
/// (no captured history) must NOT retreat the list cursor.
#[test]
#[serial]
fn wheel_up_over_preview_at_top_does_not_move_list() {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view.preview_scroll_offset = 0;
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 10;

    let handled = env.view.handle_scroll_up(50, 10);

    assert!(
        !handled,
        "expected no-op when preview has no history to reveal"
    );
    assert_eq!(env.view.cursor, 1, "list cursor must not move");
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// Wheel over preview when no session is selected must NOT move the
/// list cursor; scroll events stay in the preview pane.
#[test]
#[serial]
fn wheel_over_preview_with_no_session_does_not_move_list() {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.selected_session = None;

    let down_handled = env.view.handle_scroll_down(50, 10);
    assert!(!down_handled);
    assert_eq!(env.view.cursor, 1);

    let up_handled = env.view.handle_scroll_up(50, 10);
    assert!(!up_handled);
    assert_eq!(env.view.cursor, 1);
}

/// Wheel over preview with scrollable content moves the preview
/// offset, not the list cursor.
#[test]
#[serial]
fn wheel_over_preview_with_scrollable_content_moves_preview_only() {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 200;
    env.view.preview_scroll_offset = 10;

    let cursor_before = env.view.cursor;

    let up_handled = env.view.handle_scroll_up(50, 10);
    assert!(up_handled);
    assert_eq!(env.view.cursor, cursor_before, "list cursor must not move");
    assert!(
        env.view.preview_scroll_offset > 10,
        "preview should scroll back into history"
    );

    let offset_after_up = env.view.preview_scroll_offset;
    let down_handled = env.view.handle_scroll_down(50, 10);
    assert!(down_handled);
    assert_eq!(env.view.cursor, cursor_before, "list cursor must not move");
    assert!(
        env.view.preview_scroll_offset < offset_after_up,
        "preview should scroll forward"
    );
}

/// Wheel over the list pane still moves the list cursor (regression
/// guard so the fix above doesn't accidentally kill list scrolling).
#[test]
#[serial]
fn wheel_over_list_still_moves_list_cursor() {
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    let handled = env.view.handle_scroll_down(5, 10);
    assert!(handled);
    assert_eq!(env.view.cursor, 1, "wheel over list should advance cursor");

    let handled = env.view.handle_scroll_up(5, 10);
    assert!(handled);
    assert_eq!(env.view.cursor, 0, "wheel over list should retreat cursor");
}

/// List-pane wheel scroll stays suppressed in live mode: changing the selection would
/// silently aim the next keystroke at a different pane than the preview shows.
#[test]
#[serial]
fn wheel_over_list_in_live_mode_does_not_change_selection() {
    use crate::tui::home::live_send::LiveSendState;
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view.live_send = Some(LiveSendState {
        session_id: "fake".to_string(),
        title: "fake".to_string(),
        tmux_name: "fake".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });

    let handled = env.view.handle_scroll_down(5, 10);
    assert!(!handled, "list scroll must be a no-op in live mode");
    assert_eq!(env.view.cursor, 1, "selection must not change in live mode");
}

/// A live-send env with the default Ctrl+B leader armed and the cursor on a real session,
/// so leader-menu keys route through `handle_live_send_key`.
fn live_env_with_leader() -> TestEnv {
    use crate::tui::home::live_send::LiveSendState;
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    let id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("fixture should have a session at flat_items[1]"),
    };
    env.view.live_send = Some(LiveSendState {
        session_id: id,
        title: "session".to_string(),
        tmux_name: "fake".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: crate::tui::home::live_send::parse_chord(
            crate::tui::home::live_send::DEFAULT_LEADER,
        ),
    });
    env
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Pressing the leader arms the menu (swallowed, not forwarded);
/// the follow-up `b` toggles the sidebar and disarms.
#[test]
#[serial]
fn live_leader_b_toggles_sidebar() {
    let mut env = live_env_with_leader();
    assert!(!env.view.sidebar_collapsed);

    env.view.handle_key(ctrl('b'), None);
    assert!(
        env.view.live_send_pending_leader,
        "leader press should arm the menu"
    );
    assert!(
        !env.view.sidebar_collapsed,
        "leader alone must not toggle anything yet"
    );

    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(!env.view.live_send_pending_leader, "menu should disarm");
    assert!(env.view.sidebar_collapsed, "leader+b hides the sidebar");

    // And again to reveal it.
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(key(KeyCode::Char('b')), None);
    assert!(!env.view.sidebar_collapsed, "leader+b again shows it");
}

/// Leader follow-ups: `k` opens the palette over live mode, `q` exits, and an unbound or
/// modified key cancels the menu without firing anything (the leader swallowed it). The fast
/// exit chord (Ctrl+Q) stays a single press. None of them touch the persisted collapse.
#[test]
#[serial]
fn live_leader_follow_up_keys() {
    // (label, keys, sidebar collapsed before, still live, palette open)
    let cases = [
        (
            "leader+k",
            vec![ctrl('b'), key(KeyCode::Char('k'))],
            false,
            true,
            true,
        ),
        (
            "leader+q",
            vec![ctrl('b'), key(KeyCode::Char('q'))],
            true,
            false,
            false,
        ),
        (
            "leader+unbound",
            vec![ctrl('b'), key(KeyCode::Char('z'))],
            false,
            true,
            false,
        ),
        (
            "leader+Ctrl+K",
            vec![ctrl('b'), ctrl('k')],
            false,
            true,
            false,
        ),
        ("Ctrl+Q alone", vec![ctrl('q')], false, false, false),
    ];
    for (label, keys, collapsed, live, palette) in cases {
        let mut env = live_env_with_leader();
        env.view.sidebar_collapsed = collapsed;
        for k in keys {
            env.view.handle_key(k, None);
        }
        assert!(!env.view.live_send_pending_leader, "{label}: menu disarms");
        assert_eq!(env.view.live_send.is_some(), live, "{label}");
        assert_eq!(env.view.command_palette.is_some(), palette, "{label}");
        assert_eq!(env.view.sidebar_collapsed, collapsed, "{label}");
    }
}

/// Committing a palette command while live exits live mode first, so the preview can never
/// show one session while keystrokes target another. Cancelling is covered separately and
/// must stay live.
#[test]
#[serial]
fn palette_command_while_live_exits_live() {
    let mut env = live_env_with_leader();
    // Open the palette from within live mode via the leader.
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert!(env.view.command_palette.is_some());
    assert!(env.view.live_send.is_some(), "palette opens over live mode");

    // Filter to a jump entry and commit it.
    for ch in "jump".chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    assert!(
        env.view.live_send.is_none(),
        "committing a palette command must drop out of live mode"
    );
    assert!(env.view.command_palette.is_none());
    assert!(
        !env.view.sidebar_collapsed,
        "sidebar was never collapsed, so it stays expanded"
    );
}

/// Collapsing the sidebar in live mode hands the preview the full width: the sub-rect grows
/// past the side-by-side width and the which-key banner still renders.
#[test]
#[serial]
fn collapsed_sidebar_gives_preview_full_width() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = live_env_with_leader();
    let theme = crate::tui::styles::load_theme("empire");

    let render = |env: &mut TestEnv| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        env.view.preview_pane_area.width
    };

    let split_width = render(&mut env);
    env.view.sidebar_collapsed = true;
    let full_width = render(&mut env);
    assert!(
        full_width > split_width,
        "collapsed sidebar should widen the preview ({full_width} vs {split_width})"
    );
    // The list isn't drawn while collapsed, so its hit-test rects must be cleared or a
    // click in the preview area could resolve to a hidden list row.
    assert!(
        env.view.list_inner_area.width == 0 && env.view.list_inner_area.height == 0,
        "collapsed sidebar must clear the list hit-test rect"
    );
    assert!(
        env.view.handle_click(2, 2).is_none(),
        "a click in collapsed live mode must not resolve to a list row"
    );

    // The which-key banner renders without panicking while armed.
    env.view.live_send_pending_leader = true;
    let _ = render(&mut env);
}

/// The collapse button and the strip are click-toggle affordances: the button collapses,
/// the strip re-expands, and each reports its hit rect while the other is cleared.
#[test]
#[serial]
fn sidebar_collapse_button_and_strip_toggle() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(3);
    let theme = crate::tui::styles::load_theme("empire");

    let render = |env: &mut TestEnv| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
    };

    // Expanded: the collapse button has a real rect; clicking it collapses.
    render(&mut env);
    assert!(!env.view.sidebar_collapsed);
    let btn = env.view.collapse_button_area;
    assert!(
        btn.width > 0 && btn.height > 0,
        "collapse button must have a hit rect while expanded"
    );
    assert!(
        env.view.handle_sidebar_collapse_click(btn.x, btn.y),
        "clicking the collapse button is consumed"
    );
    assert!(
        env.view.sidebar_collapsed,
        "collapse button click collapses the sidebar"
    );

    // Collapsed: the strip has a real rect, the button rect is cleared,
    // and clicking the strip re-expands.
    render(&mut env);
    let strip = env.view.expand_strip_area;
    assert!(
        strip.width > 0 && strip.height > 0,
        "collapsed strip must have a hit rect"
    );
    assert_eq!(
        env.view.collapse_button_area,
        Rect::default(),
        "collapse button rect cleared while collapsed"
    );
    assert!(
        env.view
            .handle_sidebar_collapse_click(strip.x + 1, strip.y + 1),
        "clicking the strip is consumed"
    );
    assert!(
        !env.view.sidebar_collapsed,
        "strip click re-expands the sidebar"
    );

    // A takeover view returns early in `render` before the home paths run, so the collapse
    // and footer rects are cleared up front; a stale rect could otherwise swallow a click on
    // the takeover surface, since the collapse handler runs ahead of `hit_diff`.
    render(&mut env);
    assert!(!env.view.footer_buttons.is_empty());
    env.view.settings_view = Some(crate::tui::settings::SettingsView::new("test", None).unwrap());
    render(&mut env);
    assert_eq!(env.view.collapse_button_area, Rect::default());
    assert_eq!(env.view.expand_strip_area, Rect::default());
    assert!(env.view.footer_buttons.is_empty());
    assert!(!env.view.handle_sidebar_collapse_click(0, 0));
}

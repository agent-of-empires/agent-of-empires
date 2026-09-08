//! Wheel events are confined to whichever pane the mouse is over.
//! In particular, a wheel over the preview pane never moves the list
//! cursor: not when the preview is at its scroll boundary, and not
//! when no session is selected. See issue #1361.

use super::*;
use ratatui::layout::Rect;

fn setup_panes(env: &mut TestEnv) {
    env.view.list_area = Rect::new(0, 0, 30, 40);
    env.view.preview_area = Rect::new(30, 0, 100, 40);
}

/// Build a live-send env whose preview-capture worker reports the
/// given cursor, so the alternate-screen wheel-forwarding branch can
/// be exercised without a real full-screen pane.
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

/// Like `live_env_with_cursor` but WITHOUT entering live-send: the
/// session is merely previewed (the common "hover the preview" case).
/// No `live_send` / `live_send_worker`; the capture worker and its
/// target are set so `forward_wheel_to_preview` can take the passive
/// one-shot path.
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

/// Live-send target is a full-screen app with SGR mouse tracking on:
/// the wheel is forwarded to the app (returns to the live edge) instead
/// of growing the useless normal-buffer capture window. This is the fix
/// for the "scroll up a little then snap to the very first part of the
/// session" report on alternate-screen agents.
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

/// A full-screen app WITHOUT mouse tracking (e.g. Claude Code's
/// fullscreen renderer: `1049h` + `1007h`, no mouse) does not scroll on
/// arrow keys (it reads them as cursor / input-history navigation), so we
/// forward `PageUp`/`PageDown` named keys instead and pin the preview to
/// the live edge, just like the mouse-tracking case. Regression for #2407.
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

/// Passive preview (NOT live-send) over a full-screen agent must ALSO
/// forward the wheel. The alternate screen has no scrollback, so the
/// capture-window scroll is inert; without forwarding, "hover the
/// preview and scroll" does literally nothing (the reported regression
/// after Claude Code's fullscreen renderer landed). Forwarding pins the
/// preview to the live edge, exactly like the live-send path.
#[test]
#[serial]
fn wheel_over_alt_screen_passive_preview_forwards() {
    let mut env = passive_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(
        env.view.live_send.is_none(),
        "this exercises passive preview, not live-send"
    );

    let up = env.view.handle_scroll_up(50, 10);
    assert!(
        up,
        "wheel over a full-screen pane in passive preview is forwarded"
    );
    assert_eq!(
        env.view.preview_scroll_offset, 0,
        "passive forwarding pins the preview to the live edge"
    );

    env.view.preview_scroll_offset = 10;
    let down = env.view.handle_scroll_down(50, 10);
    assert!(down);
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// In live-send over a mouse-tracking agent, a plain (no-Shift) left
/// press/release is forwarded to the agent and consumed, and the held
/// button is tracked so its release can't be stranded.
#[test]
#[serial]
fn forward_mouse_to_preview_left_click_forwards() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Down(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    assert_eq!(env.view.mouse_forward_btn, Some(0));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Up(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    assert_eq!(env.view.mouse_forward_btn, None);
    // Forwarding never starts an aoe text selection.
    assert!(env.view.drag_state.is_none());
    assert!(env.view.preview_selection.is_none());
}

/// Passive preview (NOT live-send) over a mouse-tracking agent ALSO forwards
/// a plain press/drag/release, so hovering an agent and dragging drives its
/// native selection / scroll, exactly like the live-send case (and like the
/// passive wheel path). The one-shot send carries it with no live worker.
#[test]
#[serial]
fn forward_mouse_to_preview_passive_preview_forwards() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = passive_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(
        env.view.live_send.is_none(),
        "this exercises passive preview, not live-send"
    );
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Down(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    assert_eq!(env.view.mouse_forward_btn, Some(0));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Drag(MouseButton::Left),
        KeyModifiers::NONE,
        55,
        12
    ));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Up(MouseButton::Left),
        KeyModifiers::NONE,
        55,
        12
    ));
    assert_eq!(env.view.mouse_forward_btn, None);
    // Forwarding never starts an aoe text selection, even passively.
    assert!(env.view.drag_state.is_none());
    assert!(env.view.preview_selection.is_none());
}

/// Bare motion over the preview is forwarded to an any-event-tracking
/// (1003) agent so its hover UI (Claude Code's expandable-block
/// highlight) works in live mode, deduped per pane cell, and re-armed
/// when the pointer leaves the preview and comes back.
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
}

/// A button-tracking (1000/1002) agent never gets bare motion: it didn't
/// ask for it and would misparse the report. Same for a non-mouse agent.
#[test]
#[serial]
fn forward_hover_to_preview_requires_any_event_tracking() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);
    assert!(!env.view.forward_hover_to_preview(50, 10));
    assert_eq!(env.view.hover_forward_cell, None);

    let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
    env.view.preview_text_view.pane = Rect::new(30, 0, 100, 40);
    assert!(!env.view.forward_hover_to_preview(50, 10));
}

/// Shift+press is NOT forwarded: it falls through so aoe's own preview
/// text-selection (drag-to-copy) can run.
#[test]
#[serial]
fn forward_mouse_to_preview_shift_falls_through() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(!env.view.forward_mouse_to_preview(
        MouseEventKind::Down(MouseButton::Left),
        KeyModifiers::SHIFT,
        50,
        10
    ));
    assert_eq!(env.view.mouse_forward_btn, None);
}

/// A non-mouse agent never forwards; the event falls through to aoe.
#[test]
#[serial]
fn forward_mouse_to_preview_non_mouse_agent_falls_through() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
    assert!(!env.view.forward_mouse_to_preview(
        MouseEventKind::Down(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
}

/// Once a press is forwarded, its drag and release keep forwarding even
/// after the pointer leaves the preview rect, so the agent always sees the
/// release (no stuck button).
#[test]
#[serial]
fn forward_mouse_to_preview_drag_and_release_track_button() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Down(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    // (1, 1) is outside the preview rect, but the drag still forwards.
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Drag(MouseButton::Left),
        KeyModifiers::NONE,
        1,
        1
    ));
    assert_eq!(env.view.mouse_forward_btn, Some(0));
    assert!(env.view.forward_mouse_to_preview(
        MouseEventKind::Up(MouseButton::Left),
        KeyModifiers::NONE,
        1,
        1
    ));
    assert_eq!(env.view.mouse_forward_btn, None);
}

/// A drag or release with no forwarded press in flight is ignored (it must
/// not start forwarding mid-gesture).
#[test]
#[serial]
fn forward_mouse_to_preview_orphan_drag_ignored() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    assert!(!env.view.forward_mouse_to_preview(
        MouseEventKind::Drag(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    assert!(!env.view.forward_mouse_to_preview(
        MouseEventKind::Up(MouseButton::Left),
        KeyModifiers::NONE,
        50,
        10
    ));
    assert_eq!(env.view.mouse_forward_btn, None);
}

/// Stage an in-flight Shift-selection drag held at the preview's top
/// (`row == pane.y`) or bottom edge, plus a capture window with NO aoe-side
/// scrollback, so `tick_preview_autoscroll` exercises the agent
/// scroll-forward fallback rather than the capture-window line scroll.
fn stage_edge_drag_no_scrollback(env: &mut TestEnv, at_top: bool) {
    use crate::tui::home::PreviewTextView;
    // Visible == captured: `scroll_preview_offset` has nowhere to go, the
    // alternate-screen reality the fallback exists for. The clamp reads
    // `preview_visible_rows`, so pin it to the captured-line count to make
    // the max offset zero (no scrollback to move into).
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

/// Over a full-screen mouse-tracking agent the capture window has no
/// scrollback, so an edge-held selection forwards the same scroll input the
/// wheel does (a wheel-up/down mouse report, NOT PageUp, since the agent
/// owns the mouse) to scroll its own transcript instead of moving the inert
/// offset. The fallback delegates to `wheel_forward_key`, whose byte output
/// per branch is asserted in `wheel_forward_key_*`; here we verify the tick
/// forwards and pins the offset. Regression for the "autoscroll does nothing
/// over a mouse-tracking agent" report (PageUp was a no-op there).
#[test]
#[serial]
fn autoscroll_forwards_scroll_to_mouse_tracking_agent_at_top_edge() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    stage_edge_drag_no_scrollback(&mut env, true);
    assert!(
        env.view.tick_preview_autoscroll(),
        "top-edge tick forwards a wheel notch to the agent"
    );
    // The inert capture-window offset never moved; the agent was scrolled.
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// Same as above at the bottom edge: a wheel-down notch is forwarded.
#[test]
#[serial]
fn autoscroll_forwards_scroll_to_mouse_tracking_agent_at_bottom_edge() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, true));
    stage_edge_drag_no_scrollback(&mut env, false);
    assert!(
        env.view.tick_preview_autoscroll(),
        "bottom-edge tick forwards a wheel notch to the agent"
    );
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// A full-screen agent WITHOUT mouse tracking (Claude Code's fullscreen
/// renderer: `1049h`, no mouse) gets `PageUp`/`PageDown` from the fallback
/// instead, matching the wheel path's no-mouse branch.
#[test]
#[serial]
fn autoscroll_forwards_page_keys_to_no_mouse_agent_at_top_edge() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, false, false));
    stage_edge_drag_no_scrollback(&mut env, true);
    assert!(
        env.view.tick_preview_autoscroll(),
        "top-edge tick forwards a page key to the no-mouse agent"
    );
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// A normal-buffer pane (NOT alternate-screen) that has merely bottomed out
/// its scrollback must NOT get scroll input injected into its shell: the
/// tick is a no-op there.
#[test]
#[serial]
fn autoscroll_does_not_forward_to_normal_pane() {
    let mut env = live_env_with_cursor(alt_screen_cursor(false, false, false));
    stage_edge_drag_no_scrollback(&mut env, true);
    assert!(
        !env.view.tick_preview_autoscroll(),
        "a non-alternate-screen pane never gets forwarded scroll input"
    );
    assert_eq!(env.view.preview_scroll_offset, 0);
}

/// A full-screen app with mouse tracking but in the LEGACY (non-SGR)
/// encoding is still forwarded; the byte builder emits X10-encoded
/// bytes for it instead of SGR (see `wheel_mouse_bytes_legacy_encodes_x10`).
/// Forwarding pins the preview to the live edge like the SGR case.
#[test]
#[serial]
fn wheel_over_alt_screen_legacy_mouse_forwards() {
    let mut env = live_env_with_cursor(alt_screen_cursor(true, true, false));

    let up = env.view.handle_scroll_up(50, 10);
    assert!(up, "wheel over a full-screen legacy-mouse pane is handled");
    assert_eq!(
        env.view.preview_scroll_offset, 0,
        "legacy mouse is forwarded too (X10 encoding), not dead-scrolled"
    );
}

/// And a normal-screen agent (no alternate screen) keeps the capture
/// scroll even if it happens to have SGR mouse on: the preview's
/// scrollback is genuinely useful there.
#[test]
#[serial]
fn wheel_over_normal_screen_pane_uses_capture_scroll() {
    let mut env = live_env_with_cursor(alt_screen_cursor(false, true, true));

    let up = env.view.handle_scroll_up(50, 10);
    assert!(up);
    assert!(
        env.view.preview_scroll_offset > 10,
        "normal screen: capture-window scroll still drives the preview"
    );
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

/// Live-send mode is meant to feel like an attach — users still need
/// to scroll the preview to read agent history without exiting. The
/// has_dialog() gate would otherwise swallow these events because
/// live_send.is_some() participates in that predicate.
#[test]
#[serial]
fn wheel_over_preview_in_live_mode_scrolls_preview() {
    use crate::tui::home::live_send::LiveSendState;
    let mut env = create_test_env_with_sessions(3);
    setup_panes(&mut env);
    env.view.cursor = 1;
    env.view.update_selected();
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 200;
    env.view.preview_scroll_offset = 10;
    // Install live state directly so we don't have to stand up a
    // tmux session; the scroll handler only cares about
    // live_send.is_some().
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

    let up_handled = env.view.handle_scroll_up(50, 10);
    assert!(up_handled, "preview scroll should work while in live mode");
    assert!(
        env.view.preview_scroll_offset > 10,
        "preview should scroll back into history"
    );
    // And we should still be in live mode (scroll doesn't exit).
    assert!(env.view.live_send.is_some());
}

/// List-pane wheel scroll stays suppressed in live mode: changing
/// the selection mid-session would silently aim the next keystroke
/// at a different pane than the preview is showing.
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

/// Build a live-send env with the default Ctrl+B leader armed and the
/// cursor on a real session, so leader-menu keys route through
/// `handle_live_send_key`.
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

/// Leader + k opens the command palette over live mode.
#[test]
#[serial]
fn live_leader_k_opens_palette() {
    let mut env = live_env_with_leader();
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert!(!env.view.live_send_pending_leader);
    assert!(
        env.view.command_palette.is_some(),
        "leader+k should open the command palette"
    );
    // Live mode is still active underneath the palette overlay.
    assert!(env.view.live_send.is_some());
}

/// Leader + q exits live mode and disarms the leader menu. The sidebar
/// collapse is now a persisted general state, so exiting live mode
/// deliberately leaves it as the user set it (no force-reveal).
#[test]
#[serial]
fn live_leader_q_exits() {
    let mut env = live_env_with_leader();
    env.view.sidebar_collapsed = true;
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(key(KeyCode::Char('q')), None);
    assert!(env.view.live_send.is_none(), "leader+q exits live mode");
    assert!(
        env.view.sidebar_collapsed,
        "collapse is persisted, not reset on live exit"
    );
    assert!(!env.view.live_send_pending_leader);
}

/// An unbound key after the leader cancels the menu without exiting,
/// toggling, or opening anything (it does not fall through to the
/// agent either: the leader already swallowed it).
#[test]
#[serial]
fn live_leader_unknown_key_cancels_menu() {
    let mut env = live_env_with_leader();
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(key(KeyCode::Char('z')), None);
    assert!(!env.view.live_send_pending_leader, "menu disarms");
    assert!(env.view.live_send.is_some(), "still live");
    assert!(!env.view.sidebar_collapsed);
    assert!(env.view.command_palette.is_none());
}

/// The fast exit chord (Ctrl+Q) stays a single press, independent of
/// the leader: it must not require arming the menu first.
#[test]
#[serial]
fn live_ctrl_q_still_one_press_exit() {
    let mut env = live_env_with_leader();
    env.view.handle_key(ctrl('q'), None);
    assert!(
        env.view.live_send.is_none(),
        "Ctrl+Q exits in a single press"
    );
    assert!(!env.view.live_send_pending_leader);
}

/// A modified key after the leader (e.g. Ctrl+K) cancels the menu
/// rather than firing a command: only the leader-again passthrough
/// claims a modified form, so the user can't accidentally trigger the
/// palette by holding Ctrl out of muscle memory.
#[test]
#[serial]
fn live_leader_then_modified_key_cancels() {
    let mut env = live_env_with_leader();
    env.view.handle_key(ctrl('b'), None);
    env.view.handle_key(ctrl('k'), None);
    assert!(!env.view.live_send_pending_leader, "menu disarms");
    assert!(
        env.view.command_palette.is_none(),
        "leader + Ctrl+K must NOT open the palette"
    );
    assert!(env.view.live_send.is_some(), "still live");
}

/// Committing a palette command while live (here a jump) exits live
/// mode first, so the preview can never show one session while
/// keystrokes target another. Cancelling the palette is covered
/// separately and must stay live.
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

/// Collapsing the sidebar in live mode hands the preview the full
/// width: the preview sub-rect grows past the normal side-by-side
/// width, and rendering the which-key banner doesn't panic.
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
    // The list isn't drawn while collapsed, so its hit-test rects must
    // be cleared or a click in the preview area could resolve to a
    // hidden list row.
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

/// The collapse button (expanded) and the strip (collapsed) are
/// click-toggle affordances: clicking the button collapses, clicking
/// the strip re-expands, and each reports its hit rect while the other
/// is cleared.
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
}

/// A takeover view (settings/diff/serve) returns early in `render`
/// before the home-view paths run, so the collapse/expand and footer
/// hit rects must be cleared up front. Otherwise a stale rect from the
/// prior home frame could swallow a click on the takeover surface (the
/// collapse handler runs ahead of `hit_diff`).
#[test]
#[serial]
fn takeover_view_clears_sidebar_hit_rects() {
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

    // Home view populates the collapse button + footer rects.
    render(&mut env);
    assert!(env.view.collapse_button_area.width > 0);
    assert!(!env.view.footer_buttons.is_empty());

    // Opening settings is a full-screen takeover; the next render must
    // clear the stale rects so a click can't toggle the hidden sidebar.
    env.view.settings_view = Some(crate::tui::settings::SettingsView::new("test", None).unwrap());
    render(&mut env);
    assert_eq!(env.view.collapse_button_area, Rect::default());
    assert_eq!(env.view.expand_strip_area, Rect::default());
    assert!(env.view.footer_buttons.is_empty());
    assert!(
        !env.view.handle_sidebar_collapse_click(0, 0),
        "no sidebar rect can be hit while a takeover view owns the screen"
    );
}

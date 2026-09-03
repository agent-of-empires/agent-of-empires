//! Click-and-drag on the preview pane starts an in-app text
//! selection whenever the pane is on screen (in or out of live
//! mode). The selection is anchored to a distance from the newest
//! line, so it survives a scroll (even as the captured window grows)
//! and can span more than one page; the renderer re-derives the
//! highlight rects each frame and release copies the full range
//! through OSC 52. We need our own selection handler because the TUI
//! captures mouse events to support wheel scroll, which keeps
//! terminal-native drag-select from reaching the preview.

use super::*;
use crate::tui::home::{live_send::LiveSendState, DragKind, PreviewSelection, PreviewTextView};
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};

/// Absolute parsed-line index to the `from_bottom` distance the
/// selection stores, for a pane of `total` lines. Tests express
/// positions in absolute lines for readability and convert at the
/// boundary.
fn fb(total: usize, abs: usize) -> usize {
    total - 1 - abs
}

/// Read a stored `(col, from_bottom)` selection cell back as
/// `(col, abs_line)` for a pane of `total` lines.
fn to_abs(total: usize, cell: (u16, usize)) -> (u16, usize) {
    (cell.0, total - 1 - cell.1)
}

/// Stage the output-pane text-view snapshot the render path would set,
/// plus the backing parsed cache, so the drag handlers can map screen
/// cells to content lines and the copy can read them back. The scroll
/// offset is derived so it agrees with `first_line` (the auto-scroll
/// path projects `first_line` from the live offset, so the two must
/// line up for the staged state to be self-consistent).
fn stage_text(env: &mut TestEnv, pane: Rect, first_line: usize, lines: &[&str]) {
    let text: Text<'static> = lines.iter().map(|l| Line::from(l.to_string())).collect();
    let total_lines = text.lines.len();
    env.view.preview_cache.parsed_text = Some(text);
    env.view.preview_cache.captured_lines = total_lines;
    // `scroll_preview_offset` clamps the auto-scroll max offset against
    // `preview_visible_rows` (the rendered output-body height, same as the
    // per-frame `clamp_scroll_to_capture`); pin it to the pane height so the
    // max offset matches what `first_line` implies.
    env.view.preview_visible_rows = pane.height as usize;
    env.view.preview_cache.dimensions = (pane.width, pane.height + 1);
    env.view.preview_area = pane;
    env.view.preview_scroll_offset = total_lines
        .saturating_sub(pane.height as usize)
        .saturating_sub(first_line) as u16;
    env.view.preview_text_view = PreviewTextView {
        pane,
        first_line,
        total_lines,
    };
}

/// Stage a pane with `total_lines` of filler so coord-only tests don't
/// have to spell out line contents.
fn stage_pane(env: &mut TestEnv, pane: Rect, first_line: usize, total_lines: usize) {
    let filler: Vec<String> = (0..total_lines).map(|i| format!("line{i:04}")).collect();
    let refs: Vec<&str> = filler.iter().map(|s| s.as_str()).collect();
    stage_text(env, pane, first_line, &refs);
}

fn stage_live_send(env: &mut TestEnv) {
    // Live-send state cares only about session_id + tmux_name for
    // the parts of the home view these tests exercise (drag start
    // gate, key dismissal). The exit-chord list is unused here.
    env.view.live_send = Some(LiveSendState {
        session_id: "test-session".to_string(),
        title: "test".to_string(),
        tmux_name: "aoe_test_drag_select".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
}

#[test]
#[serial]
fn drag_start_outside_live_mode_installs_selection() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    // No live_send: a press on the preview pane still seeds a
    // PreviewSelect so users can copy from a regular session
    // preview without first entering live mode.
    assert!(env.view.handle_drag_start(50, 10));
    assert!(matches!(env.view.drag_state, Some(DragKind::PreviewSelect)));
    let sel = env.view.preview_selection.expect("selection installed");
    // col offset 50-40=10, content line first_line(0)+10=10.
    assert_eq!(to_abs(100, sel.anchor), (10, 10));
    assert_eq!(to_abs(100, sel.extent), (10, 10));
    assert!(!sel.finalized);
}

#[test]
#[serial]
fn drag_start_maps_screen_row_to_scrollback_line() {
    // With the pane scrolled into history, a press maps to the
    // absolute content line under the cursor, not the screen row.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 100, 200);
    assert!(env.view.handle_drag_start(50, 10));
    let sel = env.view.preview_selection.expect("selection installed");
    // line first_line(100) + (row 10 - pane.y 0) = 110.
    assert_eq!(to_abs(200, sel.anchor), (10, 110));
}

#[test]
#[serial]
fn drag_start_blocked_by_non_live_overlay() {
    // A modal sitting over the preview must swallow the press
    // instead of seeding a hidden highlight behind the dialog.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    env.view.show_help = true;
    assert!(!env.view.handle_drag_start(50, 10));
    assert!(env.view.preview_selection.is_none());
    assert!(env.view.drag_state.is_none());
}

#[test]
#[serial]
fn drag_start_on_empty_pane_is_noop() {
    // A pane with no captured scrollback (total_lines == 0) has
    // nothing to select, so a press there must not seed a phantom
    // selection.
    let mut env = create_test_env_empty();
    env.view.preview_text_view = PreviewTextView {
        pane: Rect::new(40, 0, 60, 20),
        first_line: 0,
        total_lines: 0,
    };
    assert!(!env.view.handle_drag_start(50, 10));
    assert!(env.view.preview_selection.is_none());
    assert!(env.view.drag_state.is_none());
}

#[test]
#[serial]
fn drag_start_below_painted_content_is_noop() {
    // Pane rows past the last painted line show no text, and
    // `screen_to_content` would clamp them onto the last line, so a
    // press there must not anchor a selection.
    let mut env = create_test_env_empty();
    let pane = Rect::new(40, 0, 60, 20);
    // (first_line, total_lines, row, starts a selection)
    let cases = [
        (0, 3, 2, true),
        (0, 3, 3, false),
        (0, 3, 10, false),
        // Scrolled into history: the window is full, so every pane
        // row is painted and the gate rejects nothing.
        (85, 105, 0, true),
        (85, 105, 19, true),
        // A partly-painted window is geometry `compute_scroll` and
        // `TranscriptGeometry` both clamp away; the gate stays right
        // without leaning on that.
        (100, 105, 4, true),
        (100, 105, 5, false),
    ];
    for (first_line, total_lines, row, accepted) in cases {
        env.view.preview_selection = None;
        env.view.drag_state = None;
        stage_pane(&mut env, pane, first_line, total_lines);
        let label = format!("first_line={first_line} total={total_lines} row={row}");
        assert_eq!(env.view.handle_drag_start(50, row), accepted, "{label}");
        assert_eq!(env.view.preview_selection.is_some(), accepted, "{label}");
        assert_eq!(
            matches!(env.view.drag_state, Some(DragKind::PreviewSelect)),
            accepted,
            "{label}"
        );
    }
}

#[test]
#[serial]
fn drag_move_below_painted_content_clamps_to_last_line() {
    // Only the start is gated: a drag already in flight that leaves
    // the painted rows keeps extending to the last content line.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 3);
    assert!(env.view.handle_drag_start(40, 0));
    assert!(env.view.handle_drag_move(45, 15));
    let sel = env.view.preview_selection.expect("selection installed");
    assert_eq!(to_abs(3, sel.anchor), (0, 0));
    assert_eq!(to_abs(3, sel.extent), (5, 2));
}

#[test]
#[serial]
fn drag_start_inside_live_mode_installs_selection() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    assert!(env.view.handle_drag_start(50, 10));
    assert!(matches!(env.view.drag_state, Some(DragKind::PreviewSelect)));
    let sel = env.view.preview_selection.expect("selection installed");
    assert_eq!(to_abs(100, sel.anchor), (10, 10));
    assert_eq!(to_abs(100, sel.extent), (10, 10));
    assert!(!sel.finalized);
}

#[test]
#[serial]
fn drag_move_maps_to_content_and_clamps_to_pane() {
    let mut env = create_test_env_empty();
    // total_lines == pane height so there is no scroll room; a drag
    // far past the bottom-right clamps to the last visible cell.
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 20);
    env.view.handle_drag_start(50, 10);
    assert!(env.view.handle_drag_move(500, 500));
    let sel = env.view.preview_selection.expect("selection still live");
    // col offset clamps to width-1 = 59, content line to the last
    // visible line first_line(0)+height-1 = 19.
    assert_eq!(to_abs(20, sel.extent), (59, 19));
}

#[test]
#[serial]
fn bare_click_collapses_to_no_selection() {
    // Down + Up with no movement should not paint a 1x1 highlight
    // or copy a single character to the clipboard.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    assert!(env.view.handle_drag_end());
    assert!(env.view.preview_selection.is_none());
    assert!(!env.view.preview_copy_pending);
}

#[test]
#[serial]
fn drag_end_finalizes_multi_cell_selection_and_arms_copy() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    assert!(env.view.handle_drag_end());
    let sel = env.view.preview_selection.expect("finalized stays");
    assert!(sel.finalized);
    // The render that paints the finalized highlight is what
    // captures the text; handle_drag_end just arms the pending flag.
    assert!(env.view.preview_copy_pending);
    assert!(env.view.preview_copy_text.is_none());
}

#[test]
#[serial]
fn keypress_in_live_mode_dismisses_finalized_selection() {
    // After release, any keystroke clears the highlight so it
    // doesn't follow agent output as the live pane refreshes.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    env.view.handle_drag_end();
    assert!(env.view.preview_selection.is_some());
    // Send a stray key through the live-send path. The session
    // doesn't exist in tmux but the dismissal happens before the
    // translate step.
    env.view.handle_key(key(KeyCode::Char('x')), None);
    assert!(env.view.preview_selection.is_none());
}

#[test]
#[serial]
fn scroll_preserves_selection() {
    // The selection is anchored to scrollback lines, so a scroll no
    // longer drops it: the highlight tracks its text as the pane
    // moves. This is what lets the user scroll to verify a copy.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    env.view.handle_drag_end();
    assert!(env.view.preview_selection.is_some());
    env.view.handle_scroll_up(50, 10);
    assert!(
        env.view.preview_selection.is_some(),
        "scroll must not clear a scrollback-anchored selection"
    );
}

#[test]
#[serial]
fn drag_move_to_bottom_edge_extends_without_scrolling() {
    // The drag-move itself only records the edge position and extends
    // to the last visible line; the scroll is the ticker's job (so a
    // held-still cursor still advances). This keeps mouse movement
    // from lurching the scroll one line per event.
    let mut env = create_test_env_empty();
    // 50 lines, 5 visible, started scrolled 10 back (showing 35..40).
    stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
    stage_live_send(&mut env);
    assert_eq!(env.view.preview_scroll_offset, 10);
    env.view.handle_drag_start(0, 0); // anchor line 35
                                      // Drag held at the bottom edge (row 4 == pane.bottom()-1).
    assert!(env.view.handle_drag_move(0, 4));
    // No scroll yet, extent pinned to the last visible line (39).
    assert_eq!(env.view.preview_scroll_offset, 10);
    assert_eq!(
        to_abs(
            50,
            env.view.preview_selection.expect("live selection").extent
        ),
        (0, 39)
    );
}

#[test]
#[serial]
fn autoscroll_tick_at_bottom_edge_scrolls_and_extends_without_new_events() {
    // The core fix: with the cursor held at the bottom edge, plain
    // ticker ticks (no further mouse events) keep scrolling toward
    // newer output and growing the selection past one visible page.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
    stage_live_send(&mut env);
    env.view.handle_drag_start(0, 0); // anchor line 35
    env.view.handle_drag_move(0, 4); // record edge position
    assert_eq!(env.view.preview_scroll_offset, 10);

    // First tick: scroll one line, extend to the new bottom (40).
    assert!(env.view.tick_preview_autoscroll());
    assert_eq!(env.view.preview_scroll_offset, 9);
    assert_eq!(
        to_abs(
            50,
            env.view.preview_selection.expect("live selection").extent
        ),
        (0, 40)
    );
    // Second tick, still no mouse event: advance again. (Clear the
    // pacing gate so the back-to-back call isn't throttled; the gate's
    // wall-clock interval is exercised in real use, not here.)
    env.view.preview_autoscroll_at = None;
    assert!(env.view.tick_preview_autoscroll());
    assert_eq!(env.view.preview_scroll_offset, 8);
    assert_eq!(
        to_abs(
            50,
            env.view.preview_selection.expect("live selection").extent
        ),
        (0, 41)
    );
}

#[test]
#[serial]
fn autoscroll_tick_at_top_edge_scrolls_into_history() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
    stage_live_send(&mut env);
    env.view.handle_drag_start(0, 4); // anchor line 39
    env.view.handle_drag_move(0, 0); // record top-edge position
    assert_eq!(env.view.preview_scroll_offset, 10);
    assert!(env.view.tick_preview_autoscroll());
    assert_eq!(env.view.preview_scroll_offset, 11);
    // New top line 34.
    assert_eq!(
        to_abs(
            50,
            env.view.preview_selection.expect("live selection").extent
        ),
        (0, 34)
    );
}

#[test]
#[serial]
fn autoscroll_tick_is_noop_off_edge_and_without_drag() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
    stage_live_send(&mut env);
    // No drag in progress yet.
    assert!(!env.view.tick_preview_autoscroll());
    // Drag held in the middle of the pane: nothing to auto-scroll.
    env.view.handle_drag_start(0, 2);
    env.view.handle_drag_move(0, 2);
    assert!(!env.view.tick_preview_autoscroll());
    assert_eq!(env.view.preview_scroll_offset, 10);
    // After release the tick must not resume scrolling.
    env.view.preview_drag_pos = Some((0, 4)); // pretend edge
    env.view.handle_drag_end();
    assert!(!env.view.tick_preview_autoscroll());
}

#[test]
#[serial]
fn extract_stays_locked_to_lines_when_capture_window_grows() {
    // Regression for the scroll-up-copies-wrong bug: live mode
    // re-captures a LARGER window as the user scrolls back, which
    // shifts every absolute line index. Because the selection is
    // anchored to the newest line, growing the window must NOT change
    // which physical lines the copy resolves to.
    let mut env = create_test_env_empty();
    // Small window: 6 lines, bottom two are E and F.
    stage_text(
        &mut env,
        Rect::new(0, 0, 5, 3),
        3,
        &["AAAAA", "BBBBB", "CCCCC", "DDDDD", "EEEEE", "FFFFF"],
    );
    // Select the bottom two lines (abs 4 and 5 in the small window).
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, fb(6, 4)),
        extent: (4, fb(6, 5)),
        finalized: true,
    });
    assert_eq!(
        env.view.extract_preview_selection_text().as_deref(),
        Some("EEEEE\nFFFFF")
    );

    // The window grows by four older lines prepended at the top (a
    // scroll-back re-capture); the same physical bottom lines are now
    // at abs 8 and 9. The stored `from_bottom` distances are untouched.
    stage_text(
        &mut env,
        Rect::new(0, 0, 5, 3),
        7,
        &[
            "qqqqq", "rrrrr", "sssss", "ttttt", "AAAAA", "BBBBB", "CCCCC", "DDDDD", "EEEEE",
            "FFFFF",
        ],
    );
    // Same physical lines, even though their absolute indices moved.
    assert_eq!(
        env.view.extract_preview_selection_text().as_deref(),
        Some("EEEEE\nFFFFF")
    );
}

#[test]
#[serial]
fn extract_spans_full_scrollback_across_pages() {
    // The core multi-page guarantee: a selection whose start has
    // scrolled off the top of the visible window still copies the
    // off-screen lines, because extraction reads the parsed cache,
    // not the visible frame buffer.
    let mut env = create_test_env_empty();
    // Pane shows 3 rows; first visible line is 3, so lines 1 and 2
    // are above the fold.
    stage_text(
        &mut env,
        Rect::new(0, 0, 10, 3),
        3,
        &[
            "line0aaa", "line1bbb", "line2ccc", "line3ddd", "line4eee", "line5fff",
        ],
    );
    // Absolute lines 1..4 (col 0 of line 1 through col 6 of line 4).
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, fb(6, 1)),
        extent: (6, fb(6, 4)),
        finalized: true,
    });
    let text = env
        .view
        .extract_preview_selection_text()
        .expect("non-empty text");
    assert_eq!(text, "line1bbb\nline2ccc\nline3ddd\nline4ee");
}

#[test]
#[serial]
fn extract_trims_trailing_whitespace_per_row() {
    let mut env = create_test_env_empty();
    stage_text(
        &mut env,
        Rect::new(0, 0, 10, 3),
        0,
        &["hello     ", "world     ", "          "],
    );
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, fb(3, 0)),
        extent: (9, fb(3, 1)),
        finalized: true,
    });
    let text = env
        .view
        .extract_preview_selection_text()
        .expect("non-empty text");
    assert_eq!(text, "hello\nworld");
}

#[test]
#[serial]
fn extract_returns_none_for_whitespace_only_selection() {
    let mut env = create_test_env_empty();
    stage_text(&mut env, Rect::new(0, 0, 5, 2), 0, &["     ", "     "]);
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (0, fb(2, 0)),
        extent: (4, fb(2, 1)),
        finalized: true,
    });
    assert!(env.view.extract_preview_selection_text().is_none());
}

#[test]
#[serial]
fn take_preview_copy_text_drains_once() {
    // The app loop reads preview_copy_text after the post-drag
    // draw; the field must yield Some once and None thereafter so
    // a stable highlight doesn't write to the clipboard on every
    // subsequent frame.
    let mut env = create_test_env_empty();
    env.view.preview_copy_text = Some("clip me".to_string());
    assert_eq!(
        env.view.take_preview_copy_text().as_deref(),
        Some("clip me")
    );
    assert!(env.view.take_preview_copy_text().is_none());
}

#[test]
#[serial]
fn real_modal_during_preview_drag_cancels_selection() {
    // Live-send counts as a dialog under has_dialog() and is what
    // makes drag-select run in the first place; it must not cancel
    // the drag. But a real modal (info / confirm / etc.) popping up
    // mid-drag must drop the selection and stop mutating state
    // behind the overlay.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    assert!(env.view.handle_drag_start(50, 10));
    assert!(env.view.handle_drag_move(55, 10));
    assert!(env.view.preview_selection.is_some());

    // Open a real modal mid-drag (info dialog as a stand-in for
    // any of the non-live-send modals that has_dialog covers).
    env.view.info_dialog = Some(super::super::super::dialogs::InfoDialog::new(
        "title", "body",
    ));
    // Next drag-move should detect the modal and cancel.
    assert!(!env.view.handle_drag_move(60, 10));
    assert!(env.view.preview_selection.is_none());
    assert!(env.view.drag_state.is_none());
    assert!(!env.view.preview_copy_pending);
}

#[test]
#[serial]
fn clear_preview_selection_drops_pending_copy() {
    // Dismissing the highlight before the render fires (e.g. a
    // keystroke between Up(Left) and the next draw) must drop the
    // pending capture so it doesn't leak into the next drag.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    env.view.handle_drag_end();
    assert!(env.view.preview_copy_pending);
    env.view.clear_preview_selection();
    assert!(!env.view.preview_copy_pending);
    assert!(env.view.preview_copy_text.is_none());
}

#[test]
#[serial]
fn flow_extract_wraps_lines_with_partial_first_and_last_rows() {
    // Tmux-style flow: anchor partway into line 0, extent partway
    // into line 2. The middle line is pulled in full, the first from
    // the anchor col onward, and the last from the left up through
    // the extent col.
    let mut env = create_test_env_empty();
    stage_text(
        &mut env,
        Rect::new(0, 0, 10, 3),
        0,
        &["abcdefghij", "klmnopqrst", "uvwxyz0123"],
    );
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (3, fb(3, 0)),
        extent: (5, fb(3, 2)),
        finalized: true,
    });
    let text = env
        .view
        .extract_preview_selection_text()
        .expect("non-empty text");
    assert_eq!(text, "defghij\nklmnopqrst\nuvwxyz");
}

#[test]
#[serial]
fn flow_extract_handles_reverse_drag() {
    // Drag from a later line up to an earlier one: anchor and extent
    // are swapped into reading order before the flow shape is built.
    let mut env = create_test_env_empty();
    stage_text(&mut env, Rect::new(0, 0, 5, 2), 0, &["abcde", "fghij"]);
    env.view.preview_selection = Some(PreviewSelection {
        anchor: (2, fb(2, 1)),
        extent: (1, fb(2, 0)),
        finalized: true,
    });
    let text = env
        .view
        .extract_preview_selection_text()
        .expect("non-empty text");
    assert_eq!(text, "bcde\nfgh");
}

/// Build a text-view snapshot for the screen-rect tests.
fn view(pane: Rect, first_line: usize, total_lines: usize) -> PreviewTextView {
    PreviewTextView {
        pane,
        first_line,
        total_lines,
    }
}

#[test]
#[serial]
fn screen_flow_rects_single_row_returns_one_segment() {
    let sel = PreviewSelection {
        anchor: (10, fb(100, 5)),
        extent: (15, fb(100, 5)),
        finalized: false,
    };
    let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0], Rect::new(10, 5, 6, 1));
}

#[test]
#[serial]
fn screen_flow_rects_two_rows_returns_two_segments() {
    let sel = PreviewSelection {
        anchor: (10, fb(100, 5)),
        extent: (3, fb(100, 6)),
        finalized: false,
    };
    let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
    assert_eq!(rects.len(), 2);
    // First line tail: cols 10..40 on row 5.
    assert_eq!(rects[0], Rect::new(10, 5, 30, 1));
    // Last line head: cols 0..=3 on row 6.
    assert_eq!(rects[1], Rect::new(0, 6, 4, 1));
}

#[test]
#[serial]
fn screen_flow_rects_three_rows_are_per_row_full_width_middles() {
    let sel = PreviewSelection {
        anchor: (10, fb(100, 5)),
        extent: (3, fb(100, 8)),
        finalized: false,
    };
    let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 20), 0, 100));
    // Per-row segments: tail, two full-width middles, head.
    assert_eq!(rects.len(), 4);
    assert_eq!(rects[0], Rect::new(10, 5, 30, 1));
    assert_eq!(rects[1], Rect::new(0, 6, 40, 1));
    assert_eq!(rects[2], Rect::new(0, 7, 40, 1));
    assert_eq!(rects[3], Rect::new(0, 8, 4, 1));
}

#[test]
#[serial]
fn screen_flow_rects_clips_rows_outside_visible_window() {
    // Selection runs from above the fold to below it; only the rows
    // inside the visible window paint, each full width since neither
    // the start nor the end line is in view.
    let sel = PreviewSelection {
        anchor: (2, fb(100, 8)),
        extent: (7, fb(100, 20)),
        finalized: false,
    };
    // Visible lines are 10..15 (first_line 10, height 5).
    let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 5), 10, 100));
    assert_eq!(rects.len(), 5);
    for (k, rect) in rects.iter().enumerate() {
        assert_eq!(*rect, Rect::new(0, k as u16, 40, 1));
    }
}

#[test]
#[serial]
fn screen_flow_rects_fully_offscreen_returns_empty() {
    let sel = PreviewSelection {
        anchor: (0, fb(100, 2)),
        extent: (5, fb(100, 4)),
        finalized: false,
    };
    // Selection lines 2..4 sit above the visible window 10..15.
    let rects = sel.screen_flow_rects(view(Rect::new(0, 0, 40, 5), 10, 100));
    assert!(rects.is_empty());
}

#[test]
#[serial]
fn full_render_pipeline_captures_copy_text_after_finalize() {
    // Drives the actual render path: render seeds the text-view
    // snapshot, the drag handlers map against it, and
    // paint_preview_selection captures the selected lines from the
    // parsed cache into `preview_copy_text` for the app loop to drain.
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");

    // Stub the preview cache so render_preview has known content to
    // paint and to back the copy.
    env.view.preview_cache.content = "alpha beta gamma\nsecond line\nthird line\n".to_string();
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 3;
    env.view.preview_cache.session_id = Some("fake-id".to_string());

    // First render seeds preview_text_view + paints content. We need
    // that before the drag handlers can map the cursor.
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    let pane = env.view.preview_text_view.pane;
    assert!(pane.width > 4, "preview pane was not set by render");
    assert!(
        env.view.preview_text_view.total_lines > 0,
        "render should have parsed scrollback into the text view"
    );

    // Install live-send so handle_drag_start claims the preview click.
    env.view.live_send = Some(LiveSendState {
        session_id: "fake-id".to_string(),
        title: "fake".to_string(),
        tmux_name: "aoe_test_full_pipeline".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    // Find a row in the output pane that actually carries text.
    let initial_buf = terminal.backend().buffer().clone();
    let mut content_row = None;
    for r in pane.y..pane.bottom() {
        let mut row_text = String::new();
        for c in pane.x..pane.right() {
            row_text.push_str(initial_buf[(c, r)].symbol());
        }
        if row_text.trim().chars().any(|ch| ch.is_alphabetic()) {
            content_row = Some(r);
            break;
        }
    }
    let row = content_row.expect("preview must paint some text");
    let start_col = pane.x;
    let end_col = pane.right() - 1;
    assert!(env.view.handle_drag_start(start_col, row));
    assert!(env.view.handle_drag_move(end_col, row));
    assert!(env.view.handle_drag_end());
    assert!(
        env.view.preview_copy_pending,
        "drag_end should arm a pending capture"
    );

    // The render that paints the finalized highlight is where the
    // capture happens: it reads the selected lines from the parsed
    // cache and stashes the string for the app loop to drain.
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    assert!(
        !env.view.preview_copy_pending,
        "render should consume the pending flag"
    );
    let copied = env
        .view
        .take_preview_copy_text()
        .expect("render should have captured selection text");
    assert!(
        copied.contains("alpha"),
        "captured text should include the first content row; got {copied:?}"
    );
}

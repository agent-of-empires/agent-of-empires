//! Click-and-drag on the preview starts an in-app text selection whenever the pane is on
//! screen, in or out of live mode. The selection is anchored to a distance from the newest
//! line, so it survives a scroll even as the captured window grows and can span more than
//! one page; the renderer re-derives the highlight each frame and release copies the full
//! range through OSC 52. The TUI captures mouse events for wheel scroll, which keeps
//! terminal-native drag-select from reaching the preview.

use super::*;
use crate::session::config::{update_config, SidebarPosition};
use crate::tui::home::{live_send::LiveSendState, DragKind, PreviewSelection, PreviewTextView};
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};

/// Absolute parsed-line index to the `from_bottom` distance the selection stores, for a
/// pane of `total` lines. Tests express positions in absolute lines and convert here.
fn fb(total: usize, abs: usize) -> usize {
    total - 1 - abs
}

/// Read a stored `(col, from_bottom)` selection cell back as
/// `(col, abs_line)` for a pane of `total` lines.
fn to_abs(total: usize, cell: (u16, usize)) -> (u16, usize) {
    (cell.0, total - 1 - cell.1)
}

/// Stage the output-pane text-view snapshot the render path would set, plus the backing
/// parsed cache, so the drag handlers can map screen cells to content lines. The scroll
/// offset is derived so it agrees with `first_line`, which the auto-scroll path projects
/// from the live offset.
fn stage_text(env: &mut TestEnv, pane: Rect, first_line: usize, lines: &[&str]) {
    let text: Text<'static> = lines.iter().map(|l| Line::from(l.to_string())).collect();
    let total_lines = text.lines.len();
    env.view.preview_cache.parsed_text = Some(text);
    env.view.preview_cache.captured_lines = total_lines;
    // `scroll_preview_offset` clamps the auto-scroll max against `preview_visible_rows`,
    // the rendered output-body height, so pin it to the pane height and the max offset
    // matches what `first_line` implies.
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
    // Live-send state matters here only for session_id and tmux_name (the drag-start gate
    // and key dismissal); the exit-chord list is unused.
    env.view.live_send = Some(LiveSendState {
        session_id: "test-session".to_string(),
        title: "test".to_string(),
        tmux_name: "aoe_test_drag_select".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
}

/// A press on the preview seeds a PreviewSelect at the absolute content line under the
/// cursor, in or out of live mode, so users can copy from a regular preview too. A modal over
/// the preview swallows the press instead of seeding a hidden highlight, and a pane with no
/// captured scrollback has nothing to select.
#[test]
#[serial]
fn drag_start_seeds_selection_at_the_pressed_content_line() {
    let pane = Rect::new(40, 0, 60, 20);
    // (label, live, modal open, first_line, total_lines, expected (col, abs line))
    let cases = [
        ("outside live mode", false, false, 0, 100, Some((10, 10))),
        ("inside live mode", true, false, 0, 100, Some((10, 10))),
        (
            "scrolled into history",
            false,
            false,
            100,
            200,
            Some((10, 110)),
        ),
        ("modal over the preview", false, true, 0, 100, None),
        ("empty pane", false, false, 0, 0, None),
    ];
    for (label, live, modal, first_line, total_lines, expected) in cases {
        let mut env = create_test_env_empty();
        if total_lines == 0 {
            env.view.preview_text_view = PreviewTextView {
                pane,
                first_line,
                total_lines,
            };
        } else {
            stage_pane(&mut env, pane, first_line, total_lines);
        }
        if live {
            stage_live_send(&mut env);
        }
        env.view.show_help = modal;
        assert_eq!(
            env.view.handle_drag_start(50, 10),
            expected.is_some(),
            "{label}"
        );
        assert_eq!(
            matches!(env.view.drag_state, Some(DragKind::PreviewSelect)),
            expected.is_some(),
            "{label}"
        );
        match expected {
            Some(cell) => {
                let sel = env.view.preview_selection.expect("selection installed");
                assert_eq!(to_abs(total_lines, sel.anchor), cell, "{label}");
                assert_eq!(to_abs(total_lines, sel.extent), cell, "{label}");
                assert!(!sel.finalized, "{label}");
            }
            None => assert!(env.view.preview_selection.is_none(), "{label}"),
        }
    }
}

/// Moving the preview cancels an unfinished selection without publishing clipboard text.
#[test]
#[serial]
fn changing_sidebar_position_cancels_preview_gesture_without_copying() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    assert!(env.view.handle_drag_start(50, 10));
    assert!(env.view.handle_drag_move(55, 11));
    env.view.try_refresh_from_config_watcher().unwrap();
    assert!(env.view.is_preview_select_dragging());
    assert!(env.view.preview_selection.is_some());

    update_config(|config| config.session.sidebar_position = SidebarPosition::Right).unwrap();
    env.view.try_refresh_from_config_watcher().unwrap();
    assert!(env.view.drag_state.is_none());
    assert!(env.view.preview_selection.is_none());
    assert!(env.view.preview_drag_pos.is_none());
    assert!(!env.view.handle_drag_move(56, 11));
    assert!(!env.view.handle_drag_end());
    assert!(!env.view.preview_copy_pending);
    assert!(env.view.take_preview_copy_text().is_none());
}

#[test]
#[serial]
fn drag_start_below_painted_content_is_noop() {
    // Pane rows past the last painted line show no text and `screen_to_content` would clamp
    // them onto the last line, so a press there must not anchor a selection.
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
        // A partly-painted window is geometry `compute_scroll` and `TranscriptGeometry`
        // both clamp away; the gate stays right without leaning on that.
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
fn drag_end_finalizes_multi_cell_selection_and_arms_copy() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    // Down + Up with no movement must not paint a 1x1 highlight or copy one character.
    env.view.handle_drag_start(50, 10);
    assert!(env.view.handle_drag_end());
    assert!(env.view.preview_selection.is_none());
    assert!(!env.view.preview_copy_pending);

    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    assert!(env.view.handle_drag_end());
    let sel = env.view.preview_selection.expect("finalized stays");
    assert!(sel.finalized);
    // The render that paints the finalized highlight is what
    // captures the text; handle_drag_end just arms the pending flag.
    assert!(env.view.preview_copy_pending);
    assert!(env.view.preview_copy_text.is_none());

    // Dismissing the highlight before the render fires drops the pending capture so it
    // doesn't leak into the next drag.
    env.view.clear_preview_selection();
    assert!(!env.view.preview_copy_pending);
    assert!(env.view.preview_copy_text.is_none());
}

#[test]
#[serial]
fn finalized_selection_survives_scroll_but_not_a_live_keypress() {
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(40, 0, 60, 20), 0, 100);
    stage_live_send(&mut env);
    env.view.handle_drag_start(50, 10);
    env.view.handle_drag_move(55, 10);
    env.view.handle_drag_end();
    // Anchored to scrollback lines, the highlight tracks its text through a scroll, which
    // is what lets the user scroll to verify a copy.
    env.view.handle_scroll_up(50, 10);
    assert!(env.view.preview_selection.is_some());
    // Any keystroke clears it so it doesn't follow agent output as the live pane refreshes.
    // The session doesn't exist in tmux, but dismissal happens before the translate step.
    env.view.handle_key(key(KeyCode::Char('x')), None);
    assert!(env.view.preview_selection.is_none());
}

#[test]
#[serial]
fn autoscroll_tick_at_bottom_edge_scrolls_and_extends_without_new_events() {
    // The core fix: with the cursor held at the bottom edge, plain ticker ticks keep
    // scrolling toward newer output and growing the selection past one page.
    let mut env = create_test_env_empty();
    stage_pane(&mut env, Rect::new(0, 0, 10, 5), 35, 50);
    stage_live_send(&mut env);
    env.view.handle_drag_start(0, 0); // anchor line 35
    env.view.handle_drag_move(0, 4); // record edge position

    // The move only records the edge and extends to the last visible line (39); scrolling
    // is the ticker's job, so movement doesn't lurch the scroll one line per event.
    assert_eq!(env.view.preview_scroll_offset, 10);
    assert_eq!(
        to_abs(
            50,
            env.view.preview_selection.expect("live selection").extent
        ),
        (0, 39)
    );

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
    // Second tick with no mouse event must advance again. Clear the pacing gate so the
    // back-to-back call isn't throttled; the wall-clock interval is exercised in real use.
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
    // Regression for the scroll-up-copies-wrong bug: live mode re-captures a larger window
    // as the user scrolls back, shifting every absolute index, but the selection is anchored
    // to the newest line, so growing the window must not change which lines the copy
    // resolves to.
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

    // The window grows by four older lines prepended at the top, so the same physical
    // bottom lines are now at abs 8 and 9 while the stored distances are untouched.
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

/// Extraction reads the parsed cache, not the frame buffer, so a selection whose start
/// scrolled off the top still copies it. It flows like tmux (partial first and last rows,
/// full middles), orders a reverse drag, trims trailing whitespace per row, and yields
/// nothing for a whitespace-only selection.
#[test]
#[serial]
fn extract_preview_selection_text_cases() {
    let mut env = create_test_env_empty();
    // (label, pane width, first_line, lines, anchor (col, abs), extent (col, abs), expected)
    type Case = (
        &'static str,
        u16,
        usize,
        &'static [&'static str],
        (u16, usize),
        (u16, usize),
        Option<&'static str>,
    );
    let cases: [Case; 5] = [
        (
            "spans lines above the fold",
            10,
            3,
            &[
                "line0aaa", "line1bbb", "line2ccc", "line3ddd", "line4eee", "line5fff",
            ],
            (0, 1),
            (6, 4),
            Some("line1bbb\nline2ccc\nline3ddd\nline4ee"),
        ),
        (
            "flow with partial first and last rows",
            10,
            0,
            &["abcdefghij", "klmnopqrst", "uvwxyz0123"],
            (3, 0),
            (5, 2),
            Some("defghij\nklmnopqrst\nuvwxyz"),
        ),
        (
            "reverse drag",
            5,
            0,
            &["abcde", "fghij"],
            (2, 1),
            (1, 0),
            Some("bcde\nfgh"),
        ),
        (
            "trailing whitespace trimmed",
            10,
            0,
            &["hello     ", "world     ", "          "],
            (0, 0),
            (9, 1),
            Some("hello\nworld"),
        ),
        (
            "whitespace only",
            5,
            0,
            &["     ", "     "],
            (0, 0),
            (4, 1),
            None,
        ),
    ];
    for (label, width, first_line, lines, anchor, extent, expected) in cases {
        let total = lines.len();
        stage_text(&mut env, Rect::new(0, 0, width, 3), first_line, lines);
        env.view.preview_selection = Some(PreviewSelection {
            anchor: (anchor.0, fb(total, anchor.1)),
            extent: (extent.0, fb(total, extent.1)),
            finalized: true,
        });
        assert_eq!(
            env.view.extract_preview_selection_text().as_deref(),
            expected,
            "{label}"
        );
    }
}

#[test]
#[serial]
fn real_modal_during_preview_drag_cancels_selection() {
    // Live-send counts as a dialog under has_dialog() and is what makes drag-select run, so
    // it must not cancel the drag. A real modal popping up mid-drag must drop the selection
    // and stop mutating state behind the overlay.
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

/// Per-row flow segments: the first line's tail, full-width middles, the last line's head,
/// clipped to the visible window (rows whose end line is off screen paint full width).
#[test]
fn screen_flow_rects_cases() {
    let row = |x, y, w| Rect::new(x, y, w, 1);
    let tall = Rect::new(0, 0, 40, 20);
    let short = Rect::new(0, 0, 40, 5);
    // (label, pane, first_line, anchor (col, abs), extent (col, abs), expected rects)
    let cases = [
        ("single row", tall, 0, (10, 5), (15, 5), vec![row(10, 5, 6)]),
        (
            "two rows",
            tall,
            0,
            (10, 5),
            (3, 6),
            vec![row(10, 5, 30), row(0, 6, 4)],
        ),
        (
            "full-width middles",
            tall,
            0,
            (10, 5),
            (3, 8),
            vec![row(10, 5, 30), row(0, 6, 40), row(0, 7, 40), row(0, 8, 4)],
        ),
        (
            "clipped to the visible window",
            short,
            10,
            (2, 8),
            (7, 20),
            (0..5).map(|y| row(0, y, 40)).collect::<Vec<_>>(),
        ),
        ("fully offscreen", short, 10, (0, 2), (5, 4), vec![]),
    ];
    for (label, pane, first_line, anchor, extent, expected) in cases {
        let sel = PreviewSelection {
            anchor: (anchor.0, fb(100, anchor.1)),
            extent: (extent.0, fb(100, extent.1)),
            finalized: false,
        };
        let rects = sel.screen_flow_rects(PreviewTextView {
            pane,
            first_line,
            total_lines: 100,
        });
        assert_eq!(rects, expected, "{label}");
    }
}

/// Finalizing a rendered selection copies the chosen text with the sidebar on either side.
#[test]
#[serial]
fn full_render_pipeline_captures_copy_text_after_finalize() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = load_theme("empire");
    let session_id = env
        .view
        .selected_session
        .clone()
        .expect("fixture must select its session");
    let title = env
        .view
        .get_instance(&session_id)
        .expect("selected instance")
        .title
        .clone();
    let tmux_name = env
        .view
        .displayed_pane_tmux_name()
        .expect("selected pane target");
    env.view.live_send = Some(LiveSendState {
        session_id: session_id.clone(),
        title,
        tmux_name: tmux_name.clone(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    env.view.preview_cache.content = "alpha beta gamma\nsecond line\nthird line\n".to_string();
    env.view.preview_cache.dimensions = (80, 24);
    env.view.preview_cache.captured_lines = 3;
    env.view.preview_cache.session_id = Some(session_id);
    env.view.preview_cache.capture_target = Some(tmux_name);

    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        env.view.sidebar_position = position;
        env.view.clear_preview_selection();
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

        let initial_buf = terminal.backend().buffer().clone();
        let mut content_cell = None;
        for r in pane.y..pane.bottom() {
            let mut row_text = String::new();
            for c in pane.x..pane.right() {
                row_text.push_str(initial_buf[(c, r)].symbol());
            }
            if let Some(offset) = row_text.find("alpha") {
                content_cell = Some((pane.x + offset as u16, r));
                break;
            }
        }
        let (start_col, row) = content_cell.expect("preview must paint seeded cache text");
        let end_col = start_col + "alpha".len() as u16 - 1;
        assert!(env.view.handle_drag_start(start_col, row));
        assert!(env.view.handle_drag_move(end_col, row));
        assert!(env.view.handle_drag_end());
        assert!(
            env.view.preview_copy_pending,
            "drag_end should arm a pending capture"
        );

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
        assert_eq!(copied, "alpha");
    }
}

use super::*;
use crate::session::config::{update_config, SidebarPosition};
use crate::tui::styles::load_theme;
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

/// Offset the viewport to expose accidental assumptions about the terminal origin.
fn render(view: &mut HomeView, width: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width + 8, 36)).unwrap();
    terminal
        .draw(|frame| {
            view.render(
                frame,
                Rect::new(4, 2, width, 32),
                &load_theme("empire"),
                None,
                None,
                None,
            );
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Both sides retain one shared border, including collapsed and diagnostics layouts.
#[test]
#[serial]
fn sidebar_position_preserves_geometry_and_single_separator() {
    let mut env = create_test_env_with_sessions(3);
    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        env.view.sidebar_position = position;
        for width in [80, 120] {
            for collapsed in [false, true] {
                for diagnostics in [false, true] {
                    env.view.sidebar_collapsed = collapsed;
                    env.view.show_diagnostics = diagnostics;
                    let buf = render(&mut env.view, width);
                    let list = if collapsed {
                        env.view.expand_strip_area
                    } else {
                        env.view.list_area
                    };
                    let preview = env.view.preview_outer_area;
                    let (seam, adjacent) = match position {
                        SidebarPosition::Left => {
                            assert_eq!(list.x, 4);
                            assert_eq!(list.right(), preview.x);
                            assert_eq!(preview.right(), 4 + width);
                            (preview.x, preview.x - 1)
                        }
                        SidebarPosition::Right => {
                            assert_eq!(preview.x, 4);
                            assert_eq!(preview.right(), list.x);
                            assert_eq!(list.right(), 4 + width);
                            (preview.right() - 1, list.x)
                        }
                    };
                    assert_eq!(buf[(seam, 6)].symbol(), "│");
                    assert_ne!(buf[(adjacent, 6)].symbol(), "│", "double border");
                    assert_eq!(env.view.divider_col, (!collapsed).then_some(seam));
                    assert!(preview.width >= crate::tui::responsive::PREVIEW_MIN_WIDTH);
                    if diagnostics {
                        assert_eq!(env.view.diagnostics_area.x, list.x);
                        assert_eq!(env.view.diagnostics_area.width, list.width);
                    }
                    if collapsed {
                        assert_eq!(env.view.list_inner_area, Rect::default());
                    } else {
                        assert_eq!(list.width, env.view.list_width);
                        assert_eq!(env.view.list_inner_area.width, list.width - 3);
                        if position == SidebarPosition::Right {
                            assert_eq!(buf[(list.right() - 1, list.y)].symbol(), "╮");
                        }
                    }
                }
            }
        }
    }
}

/// Moving the list must preserve row clicks, width controls, and collapse hit regions.
#[test]
#[serial]
fn right_sidebar_supports_selection_collapse_and_resize() {
    let mut env = create_test_env_with_sessions(3);
    env.view.sidebar_position = SidebarPosition::Right;
    render(&mut env.view, 120);

    let list = env.view.list_inner_area;
    env.view.handle_click(list.x, list.y + 2);
    assert_eq!(env.view.cursor, 2);

    let divider = env.view.divider_col.unwrap();
    assert!(env.view.handle_drag_start(divider, list.y));
    assert!(env.view.handle_drag_move(divider - 10, list.y));
    assert_eq!(env.view.list_width, 45);
    assert!(env.view.handle_drag_move(divider + 10, list.y));
    assert_eq!(env.view.list_width, 25);
    env.view.handle_drag_end();
    render(&mut env.view, 120);

    let collapse = env.view.collapse_button_area;
    assert!(env
        .view
        .handle_sidebar_collapse_click(collapse.x, collapse.y));
    let buf = render(&mut env.view, 120);
    let strip = env.view.expand_strip_area;
    assert_eq!(strip.right(), 124);
    assert!((strip.x..strip.right()).any(|x| buf[(x, strip.y + 1)].symbol() == "«"));
    assert!(env.view.handle_sidebar_collapse_click(strip.x, strip.y + 1));
    render(&mut env.view, 120);
    assert_eq!(env.view.list_area.right(), 124);
    assert_eq!(env.view.list_area.width, 25);
}

/// A drag follows rendered geometry even when saved width and viewport change.
#[test]
#[serial]
fn sidebar_divider_tracks_pointer_after_viewport_resize_and_width_clamping() {
    let mut env = create_test_env_empty();
    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        for (width, requested_width, resized_width) in [(120, 35, 140), (100, 80, 100)] {
            env.view.sidebar_position = position;
            env.view.list_width = requested_width;
            render(&mut env.view, width);
            let rendered_width = env.view.list_area.width;
            let divider = env.view.divider_col.unwrap();
            let row = env.view.list_area.y + 1;
            let col = match position {
                SidebarPosition::Left => divider - 5,
                SidebarPosition::Right => divider + 5,
            };
            assert!(env.view.handle_drag_start(divider, row));
            assert!(env.view.handle_drag_move(col, row));
            assert_eq!(env.view.list_width, rendered_width - 5);
            render(&mut env.view, resized_width);

            assert!(env.view.handle_drag_move(col - 1, row));
            render(&mut env.view, resized_width);
            assert_eq!(env.view.divider_col, Some(col - 1), "{position:?}");
            assert!(env.view.handle_drag_end());
        }
    }
}

/// Hidden and stacked layouts must not keep resizing from an obsolete drag.
#[test]
#[serial]
fn sidebar_divider_drag_ends_when_the_divider_disappears() {
    let mut env = create_test_env_empty();
    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        for collapsed in [false, true] {
            env.view.sidebar_position = position;
            env.view.sidebar_collapsed = false;
            env.view.list_width = 35;
            render(&mut env.view, 120);
            let divider = env.view.divider_col.unwrap();
            let row = env.view.list_area.y + 1;
            assert!(env.view.handle_drag_start(divider, row));
            env.view.sidebar_collapsed = collapsed;
            render(&mut env.view, if collapsed { 120 } else { 79 });
            assert_eq!(env.view.divider_col, None);

            assert!(!env.view.handle_drag_move(divider + 5, row));
            assert_eq!(env.view.list_width, 35);
            assert!(env.view.drag_state.is_none());
            assert!(!env.view.handle_drag_end());
            assert_eq!(
                crate::session::config::load_config()
                    .unwrap()
                    .unwrap()
                    .app_state
                    .home_list_width,
                Some(35)
            );
        }
    }
}

/// Temporary stacking must preserve the chosen side and requested width.
#[test]
#[serial]
fn narrow_layout_stays_stacked_and_restores_sidebar_side_on_resize() {
    let mut env = create_test_env_with_sessions(3);
    env.view.list_width = 80;
    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        env.view.sidebar_position = position;
        for width in [3, 20, 60, 79] {
            render(&mut env.view, width);
            assert_eq!(env.view.list_area.x, 4);
            assert_eq!(env.view.list_area.width, width);
            assert_eq!(env.view.list_area.bottom(), env.view.preview_outer_area.y);
            assert_eq!(env.view.preview_outer_area.width, width);
            assert_eq!(env.view.divider_col, None);
        }
        render(&mut env.view, 80);
        assert_eq!(env.view.list_area.width, 40);
        assert_eq!(env.view.preview_outer_area.width, 40);
        render(&mut env.view, 120);
        assert_eq!(env.view.list_area.width, 80);
        assert_eq!(
            env.view.list_area.x < env.view.preview_outer_area.x,
            position == SidebarPosition::Left,
        );
    }
}

/// File reloads and a fresh view resolve the same persisted preference.
#[test]
#[serial]
fn sidebar_position_reloads_and_survives_reopening() {
    let mut env = create_test_env_empty();
    render(&mut env.view, 120);
    assert_eq!(env.view.list_area.x, 4, "missing setting defaults to left");

    for position in [SidebarPosition::Right, SidebarPosition::Left] {
        update_config(|config| config.session.sidebar_position = position).unwrap();
        env.view.try_refresh_from_config_watcher().unwrap();
        render(&mut env.view, 120);
        assert_eq!(env.view.sidebar_position, position);
        assert_eq!(env.view.list_area.x == 4, position == SidebarPosition::Left);

        let mut reopened = HomeView::new_for_test(
            Some("test".to_string()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        render(&mut reopened, 120);
        assert_eq!(reopened.list_area, env.view.list_area);
    }
}

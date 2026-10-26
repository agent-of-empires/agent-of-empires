use super::*;
use crate::session::config::AppStateConfig;
use crate::tui::{responsive, styles::load_theme};
use ratatui::{backend::TestBackend, layout::Rect, Terminal};

fn render(view: &mut HomeView, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width + 4, height + 2)).unwrap();
    terminal
        .draw(|frame| {
            view.render(
                frame,
                Rect::new(4, 2, width, height),
                &load_theme("empire"),
                None,
                None,
                None,
            );
        })
        .unwrap();
}

#[test]
#[serial]
fn stacked_drag_uses_shared_border_clamps_and_survives_reopening() {
    let mut env = create_test_env_with_sessions(3);
    for diagnostics in [false, true] {
        env.view.show_diagnostics = diagnostics;
        render(&mut env.view, 60, 45);
        let row = env.view.divider_row.unwrap();
        let col = env.view.list_area.x + 20;
        assert!(env.view.hit_divider(col, row));
        for (x, y) in [(col, row - 1), (col, row + 1), (3, row), (64, row)] {
            assert!(!env.view.hit_divider(x, y), "({x}, {y})");
        }
        assert!(env.view.handle_drag_start(col, row));
        assert!(env.view.handle_drag_move(col, u16::MAX));
        render(&mut env.view, 60, 45);
        assert_eq!(
            env.view.preview_outer_area.height,
            responsive::STACKED_PREVIEW_MIN
        );
        assert!(env.view.handle_drag_move(col, 0));
        render(&mut env.view, 60, 45);
        assert_eq!(
            env.view.divider_row.unwrap() - 2,
            responsive::STACKED_LIST_HEIGHT_MIN
        );
        assert!(env.view.handle_drag_move(col, 22));
        render(&mut env.view, 60, 45);
        assert_eq!(env.view.divider_row, Some(22));
        assert!(!env.view.handle_drag_move(col, 22));
        assert!(env.view.handle_drag_end());
        assert!(!env.view.handle_drag_end());
        assert_eq!(AppStateConfig::load().unwrap().home_list_height, Some(20));
        assert_eq!(env.view.list_width, 35);
    }
    let mut reopened = test_view(Some("test"));
    render(&mut reopened, 60, 45);
    assert_eq!(reopened.divider_row, Some(22));
    render(&mut reopened, 60, 18);
    assert_eq!(
        reopened.preview_outer_area.height,
        responsive::STACKED_PREVIEW_MIN
    );
    assert_eq!(reopened.list_height, Some(20));
    render(&mut reopened, 120, 45);
    assert_eq!(reopened.list_area.width, 35);
    render(&mut reopened, 60, 45);
    assert_eq!(reopened.divider_row, Some(22));
}

#[test]
#[serial]
fn stacked_drag_stops_when_an_overlay_or_another_layout_takes_over() {
    let mut env = create_test_env_with_sessions(3);
    for takeover in 0..3 {
        env.view.sidebar_collapsed = false;
        env.view.info_dialog = None;
        env.view.list_height = None;
        render(&mut env.view, 60, 40);
        let row = env.view.divider_row.unwrap();
        assert!(env.view.handle_drag_start(30, row));
        assert!(env.view.handle_drag_move(30, 18));
        match takeover {
            0 => env.view.sidebar_collapsed = true,
            1 => env.view.info_dialog = Some(InfoDialog::new("title", "body")),
            _ => {}
        }
        render(&mut env.view, if takeover == 2 { 120 } else { 60 }, 40);
        assert!(env.view.drag_state.is_none());
        assert!(!env.view.handle_drag_move(30, 25));
        assert_eq!(AppStateConfig::load().unwrap().home_list_height, Some(16));
        if takeover == 1 {
            assert!(!env.view.hit_divider(30, env.view.divider_row.unwrap()));
        }
    }
}

#[test]
#[serial]
fn stacked_drag_works_in_live_mode() {
    let mut env = create_test_env_with_sessions(1);
    env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
        session_id: "demo".into(),
        title: "demo".into(),
        tmux_name: "demo".into(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });
    render(&mut env.view, 60, 40);
    let row = env.view.divider_row.unwrap();
    assert!(env.view.handle_drag_start(30, row));
    assert!(env.view.handle_drag_move(30, row + 3));
    assert!(env.view.handle_drag_end());
    assert_eq!(env.view.list_height, Some(row + 1));
}

#[test]
#[serial]
fn horizontal_motion_preserves_automatic_and_temporarily_clamped_height() {
    let mut env = create_test_env_with_sessions(3);
    for preference in [None, Some(20)] {
        env.view.list_height = preference;
        render(&mut env.view, 60, 18);
        let row = env.view.divider_row.unwrap();
        assert!(env.view.handle_drag_start(30, row));
        assert!(!env.view.handle_drag_move(40, row));
        assert_eq!(env.view.list_height, preference);
        assert!(env.view.handle_drag_end());
        assert_eq!(AppStateConfig::load().unwrap().home_list_height, preference);
        let mut reopened = test_view(Some("test"));
        render(&mut reopened, 60, 45);
        assert_eq!(reopened.divider_row, Some(2 + preference.unwrap_or(12)));
    }

    render(&mut env.view, 60, 45);
    let row = env.view.divider_row.unwrap();
    assert!(env.view.handle_drag_start(30, row));
    assert!(env.view.handle_drag_move(30, row - 2));
    assert!(env.view.handle_drag_move(30, row));
    assert_eq!(env.view.list_height, Some(row - 2));
    assert!(env.view.handle_drag_end());
}

//! Click-and-drag on the list/preview divider resizes `list_width`.
//! Persistence is checked via `load_config()` (the same path the
//! keyboard `<`/`>` tests exercise indirectly via save_list_width).

use super::*;
use crate::session::config::{load_config, update_config, SidebarPosition};
use ratatui::layout::Rect;

/// Stage the geometry a real side-by-side render would produce: a
/// list at column 0, divider at column 35, terminal 100 wide. The
/// list area mirrors what `render_list` would assign.
fn stage_side_by_side(env: &mut TestEnv) {
    env.view.list_area = Rect::new(0, 0, 35, 20);
    env.view.divider_col = Some(35);
    env.view.main_area_width = 100;
    env.view.list_width = 35;
}

#[test]
#[serial]
fn hit_divider_matches_only_the_divider_column() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    assert!(env.view.hit_divider(35, 5));
    assert!(!env.view.hit_divider(34, 5), "list inner shouldn't hit");
    assert!(!env.view.hit_divider(36, 5), "preview shouldn't hit");
    assert!(!env.view.hit_divider(35, 99), "row past list_area is out");
}

#[test]
#[serial]
fn hit_divider_is_false_in_stacked_mode() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    // Stacked layout clears divider_col at render time; emulate.
    env.view.divider_col = None;
    assert!(!env.view.hit_divider(35, 5));
}

#[test]
#[serial]
fn drag_updates_list_width_to_pointer() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    assert!(
        env.view.handle_drag_start(35, 5),
        "divider click starts drag"
    );
    // Drag 10 cols right.
    assert!(env.view.handle_drag_move(45, 5));
    assert_eq!(env.view.list_width, 45);
    // Drag back 5 cols (from start).
    assert!(env.view.handle_drag_move(40, 5));
    assert_eq!(env.view.list_width, 40);
}

#[test]
#[serial]
fn drag_clamps_at_preview_min_width_ceiling() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    // main_area_width=100, PREVIEW_MIN_WIDTH=40 -> ceiling=60.
    env.view.handle_drag_start(35, 5);
    env.view.handle_drag_move(200, 5);
    assert_eq!(env.view.list_width, 60);
}

#[test]
#[serial]
fn drag_clamps_at_floor_without_underflow() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    env.view.handle_drag_start(35, 5);
    // Drag far to the left of column 0; the i32 math must absorb
    // the negative without wrapping u16.
    env.view.handle_drag_move(0, 5);
    assert_eq!(env.view.list_width, 10);
}

#[test]
#[serial]
fn dialog_opening_mid_drag_ends_drag_and_persists() {
    // If a modal opens while the user is still holding the mouse
    // (e.g. a hotkey was pressed mid-drag), further Drag events must
    // not keep updating list_width invisibly under the modal. The
    // width achieved up to that point is persisted so the user's
    // work isn't silently lost on Up.
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    env.view.handle_drag_start(35, 5);
    env.view.handle_drag_move(50, 5);
    // Open a modal.
    env.view.info_dialog = Some(InfoDialog::new("title", "body"));
    // Next drag event sees the dialog and bails.
    let changed = env.view.handle_drag_move(60, 5);
    assert!(!changed);
    assert!(env.view.drag_state.is_none());
    assert_eq!(
        env.view.list_width, 50,
        "width frozen at last pre-dialog value"
    );
    let config = load_config().unwrap().expect("config saved");
    assert_eq!(config.app_state.home_list_width, Some(50));
    // Subsequent Up is now a no-op (drag_state was cleared early).
    assert!(!env.view.handle_drag_end());
}

#[test]
#[serial]
fn drag_end_persists_list_width_once() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    env.view.handle_drag_start(35, 5);
    env.view.handle_drag_move(50, 5);
    assert!(env.view.handle_drag_end());
    let config = load_config().unwrap().expect("config saved");
    assert_eq!(config.app_state.home_list_width, Some(50));
    // Subsequent Up with no active drag is a no-op.
    assert!(!env.view.handle_drag_end());
}

#[test]
#[serial]
fn drag_move_without_drag_start_is_noop() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    assert!(!env.view.handle_drag_move(50, 5));
    assert_eq!(env.view.list_width, 35);
}

#[test]
#[serial]
fn drag_start_misses_off_divider_column() {
    let mut env = create_test_env_empty();
    stage_side_by_side(&mut env);
    assert!(!env.view.handle_drag_start(34, 5));
    assert!(env.view.drag_state.is_none());
}

/// A side change commits the last width without reusing the old pointer origin.
#[test]
#[serial]
fn changing_sidebar_position_ends_drag_at_last_width() {
    let mut env = create_test_env_empty();
    for position in [SidebarPosition::Left, SidebarPosition::Right] {
        update_config(|config| config.session.sidebar_position = position).unwrap();
        env.view.try_refresh_from_config_watcher().unwrap();
        env.view.list_width = 35;
        render_geometry(&mut env.view);
        let divider = env.view.divider_col.unwrap();
        let row = env.view.list_area.y + 1;
        let drag_col = match position {
            SidebarPosition::Left => divider + 10,
            SidebarPosition::Right => divider - 10,
        };
        assert!(env.view.handle_drag_start(divider, row));
        assert!(env.view.handle_drag_move(drag_col, row));
        assert_eq!(env.view.list_width, 45);

        env.view.try_refresh_from_config_watcher().unwrap();
        assert!(
            env.view.drag_state.is_some(),
            "unchanged side keeps the drag"
        );
        assert!(!env.view.handle_drag_move(drag_col, row));

        let next_position = match position {
            SidebarPosition::Left => SidebarPosition::Right,
            SidebarPosition::Right => SidebarPosition::Left,
        };
        update_config(|config| config.session.sidebar_position = next_position).unwrap();
        env.view.try_refresh_from_config_watcher().unwrap();
        render_geometry(&mut env.view);
        assert!(!env.view.handle_drag_move(drag_col, row));
        assert_eq!(
            env.view.list_width, 45,
            "old geometry must not resize the list"
        );
        assert!(env.view.drag_state.is_none());
        assert!(!env.view.handle_drag_end());
        assert_eq!(
            load_config().unwrap().unwrap().app_state.home_list_width,
            Some(45)
        );
    }
}

/// Reversed horizontal drags obey both width limits and save the final width.
#[test]
#[serial]
fn right_divider_drag_clamps_and_persists() {
    let mut env = create_test_env_empty();
    env.view.sidebar_position = SidebarPosition::Right;
    render_geometry(&mut env.view);
    let divider = env.view.divider_col.unwrap();
    let row = env.view.list_area.y + 1;
    assert!(env.view.hit_divider(divider, row));
    assert!(!env.view.hit_list(divider, row));
    assert!(!env.view.hit_preview(divider, row));
    assert!(env.view.handle_drag_start(divider, row));
    assert!(env.view.handle_drag_move(0, row));
    assert_eq!(env.view.list_width, 80);
    assert!(env.view.handle_drag_move(u16::MAX, row));
    assert_eq!(env.view.list_width, 10);
    assert!(env.view.handle_drag_end());
    assert_eq!(
        load_config().unwrap().unwrap().app_state.home_list_width,
        Some(10)
    );
}

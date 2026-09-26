/// The Settings takeover owns the wheel and the scrollbar grab-drag. The
/// bug these guard: with settings open, `has_dialog()` is true, so the
/// generic scroll and drag paths used to swallow (scroll) or cancel
/// (drag) the gesture, leaving the fields panel dead to the mouse.
use super::*;
use crate::tui::home::DragKind;
use crate::tui::settings::SettingsView;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Render the home view (with its settings takeover) at a deliberately
/// short height so the fields panel overflows and paints a scrollbar.
fn render_short(env: &mut TestEnv) {
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
}

fn open_overflowing_settings(env: &mut TestEnv) {
    env.view.settings_view = Some(SettingsView::new("test", None).unwrap());
    render_short(env);
}

/// Locate a cell the settings scrollbar hit test accepts, so the drag
/// test doesn't hard-code render geometry.
fn scrollbar_hit(env: &TestEnv) -> Option<(u16, u16)> {
    let sv = env.view.settings_view.as_ref()?;
    for row in 0..14u16 {
        for col in 0..100u16 {
            if sv.hit_scrollbar(col, row) {
                return Some((col, row));
            }
        }
    }
    None
}

/// A scrollbar press starts a `SettingsScrollbar` drag that survives moves until release,
/// wheel-down is consumed by the fields panel, and a click off the bar falls through to
/// normal settings routing without starting a drag.
#[test]
#[serial]
fn settings_takeover_owns_wheel_and_scrollbar_drag() {
    let mut env = create_test_env_empty();
    open_overflowing_settings(&mut env);
    let (col, row) = scrollbar_hit(&env).expect("fields must overflow so a scrollbar renders");

    assert!(
        env.view.handle_dialog_click(col, row),
        "a scrollbar press is consumed by the settings takeover"
    );
    assert!(
        matches!(env.view.drag_state, Some(DragKind::SettingsScrollbar)),
        "the press seeds a scrollbar drag"
    );
    env.view.handle_drag_move(col, row.saturating_add(2));
    assert!(
        matches!(env.view.drag_state, Some(DragKind::SettingsScrollbar)),
        "the drag must survive a move even though has_dialog() is true"
    );
    env.view.handle_drag_end();
    assert!(env.view.drag_state.is_none(), "release ends the drag");

    assert!(
        env.view.handle_scroll_down(10, 10),
        "a wheel-down must scroll the settings fields panel"
    );

    // Column 2 is deep in the categories panel, nowhere near the bar.
    assert!(env.view.handle_dialog_click(2, 6));
    assert!(
        env.view.drag_state.is_none(),
        "a click off the bar must not begin a scrollbar drag"
    );
}

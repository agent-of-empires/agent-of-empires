//! Clicks on keyboard-driven dialogs replay their key through `handle_key`, so
//! the mouse reaches the same result handling as the keyboard.

use super::*;
use crate::tui::dialogs::{PermissionResponseDialog, WorktreeNameDialog};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

fn render(env: &mut TestEnv) -> Buffer {
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
#[serial]
fn a_clicked_hint_closes_its_dialog_through_the_key_path() {
    use crate::tui::dialogs::test_render::find;
    let mut env = create_test_env_with_sessions(1);

    env.view.worktree_name_dialog = Some(WorktreeNameDialog::new("dir", "branch"));
    let buf = render(&mut env);
    let (x, y) = find(&buf, "Esc cancel");
    assert!(env.view.handle_hover(x, y));
    assert!(env.view.handle_dialog_click(x, y));
    assert!(env.view.worktree_name_dialog.is_none());

    env.view.permission_response_dialog = Some(PermissionResponseDialog::new("s", None));
    // Deny resolves through the same handler as the `d` key; with no pending
    // target it just closes the dialog.
    let buf = render(&mut env);
    let (x, y) = find(&buf, "[Deny]");
    assert!(env.view.handle_dialog_click(x, y));
    assert!(env.view.permission_response_dialog.is_none());
}

#[test]
#[serial]
fn the_help_overlay_takes_the_wheel_and_closes_on_click() {
    let mut env = create_test_env_with_sessions(1);
    env.view.show_help = true;
    render(&mut env);
    assert!(env.view.owns_wheel());
    assert!(env.view.handle_scroll_down(0, 0));
    assert_eq!(env.view.help_scroll, 3);
    assert!(env.view.handle_scroll_up(0, 0));
    assert_eq!(env.view.help_scroll, 0);

    assert!(env.view.handle_dialog_click(5, 5));
    assert!(!env.view.show_help);
}

#[test]
#[serial]
fn a_click_in_the_diff_file_list_selects_that_file() {
    use crate::tui::dialogs::test_render::find;
    let mut env = create_test_env_with_sessions(1);
    let mut diff = crate::tui::diff::DiffView::test_default();
    diff.files = ["alpha.rs", "beta.rs"]
        .map(|path| crate::git::diff::DiffFile {
            path: std::path::PathBuf::from(path),
            old_path: None,
            status: crate::git::diff::FileStatus::Modified,
            additions: 0,
            deletions: 0,
        })
        .to_vec();
    env.view.diff_view = Some(diff);
    let buf = render(&mut env);
    let (x, y) = find(&buf, "beta.rs");
    assert!(env.view.handle_dialog_click(x, y));
    assert_eq!(env.view.diff_view.as_ref().unwrap().selected_file, 1);
}

#[test]
#[serial]
fn a_follow_up_dialog_over_new_session_takes_its_own_clicks() {
    use crate::tui::dialogs::test_render::find;
    let mut env = create_test_env_with_sessions(1);
    env.view.open_new_session_dialog();
    env.view.hooks_install_dialog = Some(crate::tui::dialogs::HooksInstallDialog::new("claude"));
    let buf = render(&mut env);
    let (x, y) = find(&buf, "[Cancel (Esc)]");
    assert!(env.view.handle_dialog_click(x, y));
    assert!(env.view.hooks_install_dialog.is_none());
    assert!(env.view.new_dialog.is_some());
}

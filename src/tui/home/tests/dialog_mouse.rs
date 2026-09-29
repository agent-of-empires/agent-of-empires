//! Clicks on keyboard-driven dialogs replay their key through `handle_key`, so
//! the mouse reaches the same result handling as the keyboard.

use super::*;
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

/// Each keyboard-driven dialog's drawn close/decline control, hovered then
/// clicked through `HomeView`, closes it via its own key handling.
#[test]
#[serial]
fn a_clicked_hint_closes_its_dialog_through_the_key_path() {
    use crate::session::config::SortOrder;
    use crate::tui::dialogs::test_render::find;
    use crate::tui::dialogs::*;

    type Open = fn(&mut HomeView);
    type IsOpen = fn(&HomeView) -> bool;
    let cases: &[(&str, Open, IsOpen)] = &[
        (
            "Esc cancel",
            |v| v.worktree_name_dialog = Some(WorktreeNameDialog::new("dir", "branch")),
            |v| v.worktree_name_dialog.is_some(),
        ),
        (
            "[Deny]",
            |v| v.permission_response_dialog = Some(PermissionResponseDialog::new("s", None)),
            |v| v.permission_response_dialog.is_some(),
        ),
        (
            "Esc cancel",
            |v| v.send_message_dialog = Some(SendMessageDialog::new("s")),
            |v| v.send_message_dialog.is_some(),
        ),
        (
            "Esc cancel",
            |v| {
                let profiles = vec!["default".to_string()];
                v.rename_dialog = Some(RenameDialog::new("t", "", "default", profiles, vec![]));
            },
            |v| v.rename_dialog.is_some(),
        ),
        (
            "Esc close",
            |v| v.tips_dialog = Some(TipsDialog::new(vec![], vec![], false, false)),
            |v| v.tips_dialog.is_some(),
        ),
        (
            "Esc close",
            |v| {
                let entry = ProfileEntry {
                    name: "default".to_string(),
                    session_count: 0,
                    is_active: true,
                };
                v.profile_picker_dialog = Some(ProfilePickerDialog::new(vec![entry], "default"));
            },
            |v| v.profile_picker_dialog.is_some(),
        ),
        (
            "q/Esc close",
            |v| v.projects_dialog = Some(ProjectsDialog::new("test")),
            |v| v.projects_dialog.is_some(),
        ),
        (
            "esc close",
            |v| v.skills_manager_dialog = Some(SkillsManagerDialog::new()),
            |v| v.skills_manager_dialog.is_some(),
        ),
        (
            "esc close",
            |v| v.plugin_manager_dialog = Some(PluginManagerDialog::new()),
            |v| v.plugin_manager_dialog.is_some(),
        ),
        (
            "Esc close",
            |v| v.sort_picker_dialog = Some(SortPickerDialog::new(SortOrder::Newest)),
            |v| v.sort_picker_dialog.is_some(),
        ),
    ];
    let mut env = create_test_env_with_sessions(1);
    for (i, (target, open, is_open)) in cases.iter().enumerate() {
        open(&mut env.view);
        let buf = render(&mut env);
        let (x, y) = find(&buf, target);
        assert!(
            env.view.handle_hover(x, y),
            "case {i}: hover lights {target}"
        );
        assert!(env.view.handle_dialog_click(x, y));
        assert!(!is_open(&env.view), "case {i}: {target} closes its dialog");
    }
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

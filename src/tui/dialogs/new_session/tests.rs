use super::*;
use crate::session::{merge_configs, Config, ProfileConfig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs;

const TEST_PATH: &str = ".";

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn alt_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

fn shift_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn single_tool_dialog() -> NewSessionDialog {
    NewSessionDialog::new_with_tools(vec!["claude"], TEST_PATH.to_string())
}

fn multi_tool_dialog() -> NewSessionDialog {
    NewSessionDialog::new_with_tools(vec!["claude", "opencode"], TEST_PATH.to_string())
}

#[test]
fn test_initial_state() {
    let dialog = single_tool_dialog();
    assert_eq!(dialog.title.value(), "");
    assert_eq!(dialog.path.value(), TEST_PATH);
    assert_eq!(dialog.group.value(), "");
    assert_eq!(dialog.focused_field, 0);
    assert_eq!(dialog.tool_index, 0);
    assert_eq!(dialog.profile_index, 0);
    assert_eq!(dialog.selected_profile(), "default");
}

#[test]
fn test_esc_cancels() {
    let mut dialog = single_tool_dialog();
    let result = dialog.handle_key(key(KeyCode::Esc));
    assert!(matches!(result, DialogResult::Cancel));
}

#[test]
fn test_enter_submits_with_empty_title_for_builder() {
    let mut dialog = single_tool_dialog();
    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert_eq!(data.title, "", "Empty title should pass through to builder");
            assert_eq!(data.path, TEST_PATH);
            assert_eq!(data.group, "");
            assert_eq!(data.tool, "claude");
            assert_eq!(data.profile, "default");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_enter_preserves_custom_title() {
    let mut dialog = single_tool_dialog();
    dialog.title = Input::new("My Custom Title".to_string());
    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert_eq!(data.title, "My Custom Title");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_tab_cycles_fields_single_tool() {
    let mut dialog = single_tool_dialog();
    assert_eq!(dialog.focused_field, 0); // path (single profile, no profile field)

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 1); // title

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 2); // yolo mode

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 3); // worktree

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 4); // group

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 0); // wrap to start
}

#[test]
fn test_tab_cycles_fields_single_tool_with_worktree() {
    // Even with worktree enabled, name, new_branch, and extra_repos are in a Ctrl+P overlay,
    // so the main form has the same tab stops as without worktree.
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    assert_eq!(dialog.focused_field, 0); // path

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 1); // title

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 2); // yolo mode

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 3); // worktree

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 4); // group

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 0); // wrap to start
}

#[test]
fn test_tab_cycles_fields_multi_tool() {
    let mut dialog = multi_tool_dialog();
    assert_eq!(dialog.focused_field, 0); // path

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 1); // title

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 2); // tool selection

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 3); // yolo mode

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 4); // worktree branch

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 5); // group

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 0); // wrap to start (no new_branch without worktree)
}

#[test]
fn test_backtab_cycles_fields_reverse() {
    let mut dialog = single_tool_dialog();
    assert_eq!(dialog.focused_field, 0); // path

    dialog.handle_key(shift_key(KeyCode::BackTab));
    assert_eq!(dialog.focused_field, 4); // group (last field without worktree/docker)

    dialog.handle_key(shift_key(KeyCode::BackTab));
    assert_eq!(dialog.focused_field, 3); // worktree branch

    dialog.handle_key(shift_key(KeyCode::BackTab));
    assert_eq!(dialog.focused_field, 2); // yolo mode

    dialog.handle_key(shift_key(KeyCode::BackTab));
    assert_eq!(dialog.focused_field, 1); // title

    dialog.handle_key(shift_key(KeyCode::BackTab));
    assert_eq!(dialog.focused_field, 0); // path
}

#[test]
fn test_char_input_to_title() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 1; // title
    dialog.handle_key(key(KeyCode::Char('H')));
    dialog.handle_key(key(KeyCode::Char('i')));
    assert_eq!(dialog.title.value(), "Hi");
}

#[test]
fn test_char_input_to_path() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.handle_key(key(KeyCode::Char('/')));
    dialog.handle_key(key(KeyCode::Char('a')));
    assert_eq!(dialog.path.value(), format!("{TEST_PATH}/a"));
}

#[test]
fn test_ghost_text_appears_for_single_match() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("project-alpha")).expect("failed to create directory");
    fs::write(tmp.path().join("project-file"), "not a directory").expect("failed to write file");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/pro", tmp.path().display()));
    dialog.recompute_path_ghost();

    assert_eq!(dialog.ghost_text(), Some("ject-alpha/"));
}

#[test]
fn test_ghost_text_shows_common_prefix_for_multiple_matches() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("client-api")).expect("failed to create directory");
    fs::create_dir(tmp.path().join("client-web")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/cl", tmp.path().display()));
    dialog.recompute_path_ghost();

    assert_eq!(dialog.ghost_text(), Some("ient-"));
}

#[test]
fn test_ghost_text_none_when_no_matches() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/zzz_nonexistent", tmp.path().display()));
    dialog.recompute_path_ghost();

    assert_eq!(dialog.ghost_text(), None);
}

#[test]
fn test_ghost_shows_slash_for_exact_directory_match() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/alpha", tmp.path().display()));
    dialog.recompute_path_ghost();

    assert_eq!(dialog.ghost_text(), Some("/"));
}

#[test]
fn test_right_arrow_accepts_ghost_text() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("project-alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/pro", tmp.path().display()));
    dialog.recompute_path_ghost();
    assert!(dialog.ghost_text().is_some());

    dialog.handle_key(key(KeyCode::Right));

    assert_eq!(
        dialog.path.value(),
        format!("{}/project-alpha/", tmp.path().display())
    );
}

#[test]
fn test_end_key_accepts_ghost_text() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("project-alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/pro", tmp.path().display()));
    dialog.recompute_path_ghost();
    assert!(dialog.ghost_text().is_some());

    dialog.handle_key(key(KeyCode::End));

    assert_eq!(
        dialog.path.value(),
        format!("{}/project-alpha/", tmp.path().display())
    );
}

#[test]
fn test_right_arrow_at_mid_input_moves_cursor_normally() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new("/tmp/alpha/beta".to_string());
    // Move cursor to start
    dialog.handle_key(ctrl_key(KeyCode::Char('a')));
    let cursor_before = dialog.path.visual_cursor();

    dialog.handle_key(key(KeyCode::Right));
    let cursor_after = dialog.path.visual_cursor();

    // Cursor should have moved right by 1 (normal behavior)
    assert_eq!(cursor_after, cursor_before + 1);
}

#[test]
fn test_ghost_recomputes_after_accepting() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("alpha")).expect("failed to create directory");
    fs::create_dir(tmp.path().join("alpha").join("inner")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/alp", tmp.path().display()));
    dialog.recompute_path_ghost();
    assert_eq!(dialog.ghost_text(), Some("ha/"));

    dialog.handle_key(key(KeyCode::Right)); // accept ghost

    assert_eq!(
        dialog.path.value(),
        format!("{}/alpha/", tmp.path().display())
    );
    // Ghost should have been recomputed for the next level
    assert_eq!(dialog.ghost_text(), Some("inner/"));
}

#[test]
fn test_tab_always_navigates_from_path_field() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("project-alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/pro", tmp.path().display()));
    dialog.recompute_path_ghost();
    assert!(dialog.ghost_text().is_some());

    dialog.handle_key(key(KeyCode::Tab));

    // Tab should navigate to next field, not accept ghost
    assert_eq!(dialog.focused_field, 1); // title
}

#[test]
fn test_ghost_cleared_when_leaving_path_field() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("project-alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/pro", tmp.path().display()));
    dialog.recompute_path_ghost();
    assert!(dialog.ghost_text().is_some());

    dialog.handle_key(key(KeyCode::Tab));

    assert_eq!(dialog.ghost_text(), None);
}

#[test]
fn test_ghost_not_shown_when_cursor_not_at_end() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    fs::create_dir(tmp.path().join("alpha")).expect("failed to create directory");

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new(format!("{}/alp", tmp.path().display()));
    // Move cursor to start
    dialog.handle_key(ctrl_key(KeyCode::Char('a')));
    dialog.recompute_path_ghost();

    assert_eq!(dialog.ghost_text(), None);
}

#[test]
fn test_invalid_path_flash_expires_after_tick() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path_invalid_flash_until =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
    assert!(dialog.tick());
    assert!(!dialog.is_path_invalid_flash_active());
}

#[test]
fn test_ctrl_left_jumps_to_previous_path_segment() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new("/tmp/alpha/beta".to_string());

    dialog.handle_key(ctrl_key(KeyCode::Left));
    dialog.handle_key(key(KeyCode::Char('X')));

    assert_eq!(dialog.path.value(), "/tmp/alpha/Xbeta");
}

#[test]
fn test_alt_b_jumps_to_previous_path_segment() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new("/tmp/alpha/beta".to_string());

    dialog.handle_key(alt_key(KeyCode::Char('b')));
    dialog.handle_key(key(KeyCode::Char('X')));

    assert_eq!(dialog.path.value(), "/tmp/alpha/Xbeta");
}

#[test]
fn test_ctrl_a_jumps_to_start_of_path() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0; // path
    dialog.path = Input::new("/tmp/alpha/beta".to_string());

    dialog.handle_key(ctrl_key(KeyCode::Char('a')));
    dialog.handle_key(key(KeyCode::Char('X')));

    assert_eq!(dialog.path.value(), "X/tmp/alpha/beta");
}

#[test]
fn test_char_input_to_group() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 4; // group (single tool, single profile: path=0, title=1, yolo=2, worktree=3, group=4)
    dialog.handle_key(key(KeyCode::Char('w')));
    dialog.handle_key(key(KeyCode::Char('o')));
    dialog.handle_key(key(KeyCode::Char('r')));
    dialog.handle_key(key(KeyCode::Char('k')));
    assert_eq!(dialog.group.value(), "work");
}

#[test]
fn test_backspace_removes_char() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 1; // title
    dialog.title = Input::new("Hello".to_string());
    dialog.handle_key(key(KeyCode::Backspace));
    assert_eq!(dialog.title.value(), "Hell");
}

#[test]
fn test_backspace_on_empty_field() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 1; // title
    dialog.handle_key(key(KeyCode::Backspace));
    assert_eq!(dialog.title.value(), "");
}

#[test]
fn test_tool_selection_left_right() {
    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 2; // tool field (single profile: path=0, title=1, tool=2)
    assert_eq!(dialog.tool_index, 0);

    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.tool_index, 1);

    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.tool_index, 0);

    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 1);
}

#[test]
fn test_tool_selection_left_right_three_tools() {
    let mut dialog = NewSessionDialog::new_with_tools(
        vec!["claude", "opencode", "codex"],
        TEST_PATH.to_string(),
    );
    dialog.focused_field = 2; // tool field
    assert_eq!(dialog.tool_index, 0);

    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.tool_index, 1);
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.tool_index, 2);
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.tool_index, 0, "right wraps from last to first");

    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 2, "left wraps from first to last");
    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 1);
    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 0);
}

#[test]
fn test_tool_selection_space() {
    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 2; // tool field
    assert_eq!(dialog.tool_index, 0);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(dialog.tool_index, 1);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(dialog.tool_index, 0);
}

#[test]
fn test_tool_selection_ignored_on_text_field() {
    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 1; // title
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(dialog.title.value(), " ");
    assert_eq!(dialog.tool_index, 0);
}

#[test]
fn test_tool_selection_ignored_single_tool() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 2; // yolo in single-tool mode (tool not interactive)
    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 0);
}

#[test]
fn test_submit_with_selected_tool() {
    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 2; // tool field
    dialog.handle_key(key(KeyCode::Right));
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert_eq!(data.tool, "opencode");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_unknown_key_continues() {
    let mut dialog = single_tool_dialog();
    let result = dialog.handle_key(key(KeyCode::F(1)));
    assert!(matches!(result, DialogResult::Continue));
}

#[test]
fn test_error_clears_on_input() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 1; // title
    dialog.error_message = Some("Some error".to_string());

    dialog.handle_key(key(KeyCode::Char('a')));
    assert_eq!(dialog.error_message, None);
}

#[test]
fn test_esc_clears_error() {
    let mut dialog = single_tool_dialog();
    dialog.error_message = Some("Some error".to_string());

    let result = dialog.handle_key(key(KeyCode::Esc));
    assert!(matches!(result, DialogResult::Cancel));
    assert_eq!(dialog.error_message, None);
}

#[test]
fn test_new_branch_checkbox_default_true() {
    let dialog = single_tool_dialog();
    assert!(dialog.create_new_branch);
}

#[test]
fn test_new_branch_checkbox_toggle() {
    let mut dialog = single_tool_dialog();
    // New branch is now in the worktree config overlay (Ctrl+P on worktree field)
    dialog.focused_field = 3; // worktree field
    dialog.handle_key(ctrl_key(KeyCode::Char('p'))); // Open config overlay
    assert!(dialog.worktree_config_mode);
    assert_eq!(dialog.worktree_config_focused_field, 0); // name
    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.worktree_config_focused_field, 1); // new_branch
    assert!(dialog.create_new_branch);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.create_new_branch);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.create_new_branch);
}

#[test]
fn test_submit_respects_create_new_branch() {
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.worktree_branch = Input::new("feature-branch".to_string());
    // Toggle new_branch off via config overlay
    dialog.focused_field = 3; // worktree field
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    dialog.handle_key(key(KeyCode::Tab)); // Focus new_branch
    dialog.handle_key(key(KeyCode::Char(' '))); // Toggle off
    dialog.handle_key(key(KeyCode::Esc)); // Exit overlay

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(!data.create_new_branch);
            assert!(data.worktree_enabled);
            assert_eq!(data.worktree_branch.as_deref(), Some("feature-branch"));
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_new_branch_field_hidden_without_worktree() {
    let mut dialog = single_tool_dialog();
    assert_eq!(dialog.focused_field, 0);

    // Tab through (single profile): title(0) -> path(1) -> yolo(2) -> worktree(3) -> group(4) -> wrap to 0
    dialog.handle_key(key(KeyCode::Tab)); // 1 (path)
    dialog.handle_key(key(KeyCode::Tab)); // 2 (yolo)
    dialog.handle_key(key(KeyCode::Tab)); // 3 (worktree)
    dialog.handle_key(key(KeyCode::Tab)); // 4 (group)
    assert_eq!(dialog.focused_field, 4);
    dialog.handle_key(key(KeyCode::Tab)); // Should wrap to 0
    assert_eq!(dialog.focused_field, 0);
}

#[test]
fn test_sandbox_disabled_by_default() {
    let dialog = multi_tool_dialog();
    assert!(!dialog.sandbox_enabled);
}

#[test]
fn test_worktree_disabled_by_default() {
    let dialog = multi_tool_dialog();
    assert!(!dialog.worktree_enabled);
}

#[test]
fn test_worktree_enabled_from_config() {
    let mut config = Config::default();
    config.worktree.enabled = true;

    let dialog =
        NewSessionDialog::new_with_config(vec!["claude"], "/tmp/project".to_string(), config);

    assert!(dialog.worktree_enabled);
}

#[test]
fn test_worktree_toggle_submit_without_name() {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 3; // worktree field

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.worktree_enabled);

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.worktree_enabled);
            assert!(data.worktree_branch.is_none());
            assert!(data.extra_repo_paths.is_empty());
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_worktree_config_name_field_sets_branch_override() {
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.focused_field = 3; // worktree field

    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(dialog.worktree_config_mode);
    assert_eq!(dialog.worktree_config_focused_field, 0); // name

    for ch in "feature-name".chars() {
        dialog.handle_key(key(KeyCode::Char(ch)));
    }
    dialog.handle_key(key(KeyCode::Enter));

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.worktree_enabled);
            assert_eq!(data.worktree_branch.as_deref(), Some("feature-name"));
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_sandbox_image_initialized_with_effective_default() {
    use crate::containers;
    let dialog = multi_tool_dialog();
    assert_eq!(
        dialog.sandbox_image.value(),
        containers::get_container_runtime().effective_default_image()
    );
}

#[test]
fn test_tab_skips_sandbox_options_in_main_form() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;

    // With sandbox enabled, sandbox sub-options are in separate mode now.
    // Main form (single profile): title(0), path(1), tool(2), yolo(3), worktree(4), sandbox(5), group(6)
    for _ in 0..5 {
        dialog.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(dialog.focused_field, 5); // sandbox field

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 6); // group field (no sandbox sub-options inline)

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 0); // wrap to start
}

#[test]
fn test_tab_skips_sandbox_when_disabled() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = false;

    // Single profile: title(0), path(1), tool(2), yolo(3), worktree(4), sandbox(5), group(6)
    for _ in 0..5 {
        dialog.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(dialog.focused_field, 5); // sandbox field

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 6); // group field

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 0); // wrap to start
}

#[test]
fn test_submit_with_custom_sandbox_image() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.sandbox_image = Input::new("custom/image:tag".to_string());
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.sandbox);
            assert_eq!(data.sandbox_image, "custom/image:tag");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_submit_with_default_image_passes_through() {
    use crate::containers;
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.sandbox);
            assert_eq!(
                data.sandbox_image,
                containers::get_container_runtime().effective_default_image()
            );
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_submit_with_empty_image() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.sandbox_image = Input::new("".to_string());
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.sandbox);
            assert_eq!(data.sandbox_image, "");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_submit_sandbox_image_always_included() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = false;
    dialog.sandbox_image = Input::new("custom/image:tag".to_string());
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(!data.sandbox);
            assert_eq!(data.sandbox_image, "custom/image:tag");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_sandbox_image_input_in_config_mode() {
    use crate::containers;
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 0; // image field

    dialog.handle_key(key(KeyCode::Char('a')));
    dialog.handle_key(key(KeyCode::Char('b')));
    dialog.handle_key(key(KeyCode::Char('c')));

    let expected = format!(
        "{}abc",
        containers::get_container_runtime().effective_default_image()
    );
    assert_eq!(dialog.sandbox_image.value(), expected);
}

#[test]
fn test_yolo_mode_disabled_by_default() {
    let dialog = multi_tool_dialog();
    assert!(!dialog.yolo_mode);
}

#[test]
fn test_yolo_mode_toggle() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.focused_field = 3; // yolo mode field (single profile: path=0, title=1, tool=2, yolo=3)
    assert!(!dialog.yolo_mode);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.yolo_mode);

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.yolo_mode);
}

#[test]
fn test_submit_with_yolo_mode_enabled() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.yolo_mode = true;
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.sandbox);
            assert!(data.yolo_mode);
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_yolo_independent_of_sandbox() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = false;
    dialog.yolo_mode = true;
    dialog.title = Input::new("Test".to_string());

    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(!data.sandbox);
            assert!(data.yolo_mode);
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
fn test_disabling_sandbox_does_not_reset_yolo_mode() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.yolo_mode = true;
    // sandbox field (single profile): title=0, path=1, tool=2, yolo=3, worktree=4, sandbox=5
    dialog.focused_field = 5;

    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.sandbox_enabled);
    assert!(dialog.yolo_mode);
}

#[test]
fn help_content_fits_in_dialog() {
    const BORDER_WIDTH: u16 = 2;
    const INDENT: usize = 2;
    let available_width = (HELP_DIALOG_WIDTH - BORDER_WIDTH) as usize;

    for help in FIELD_HELP {
        let line_width = INDENT + help.description.len();
        assert!(
            line_width <= available_width,
            "Help for '{}': description '{}' exceeds dialog width ({} > {})",
            help.name,
            help.description,
            line_width,
            available_width
        );
    }
}

#[test]
fn test_profile_override_sets_default_tool() {
    let global = Config::default();
    let profile_config: ProfileConfig =
        serde_json::from_value(serde_json::json!({"session": {"default_tool": "opencode"}}))
            .unwrap();

    let resolved = merge_configs(global, &profile_config);
    let dialog = NewSessionDialog::new_with_config(
        vec!["claude", "opencode"],
        "/tmp/project".to_string(),
        resolved,
    );

    assert_eq!(
        dialog.tool_index, 1,
        "Profile override should select opencode (index 1)"
    );
    assert_eq!(dialog.available_tools[dialog.tool_index], "opencode");
}

#[test]
fn test_profile_override_beats_global_default_tool() {
    let mut global = Config::default();
    global.session.default_tool = Some("claude".to_string());

    let profile_config: ProfileConfig =
        serde_json::from_value(serde_json::json!({"session": {"default_tool": "opencode"}}))
            .unwrap();

    let resolved = merge_configs(global, &profile_config);
    assert_eq!(
        resolved.session.default_tool.as_deref(),
        Some("opencode"),
        "Profile override should take precedence over global default"
    );

    let dialog = NewSessionDialog::new_with_config(
        vec!["claude", "opencode"],
        "/tmp/project".to_string(),
        resolved,
    );

    assert_eq!(
        dialog.tool_index, 1,
        "Profile override should select opencode over global claude"
    );
    assert_eq!(dialog.available_tools[dialog.tool_index], "opencode");
}

// --- confirm_create_dir tests ---

fn nonexistent_dialog() -> NewSessionDialog {
    NewSessionDialog::new_with_tools(vec!["claude"], "/__aoe_nonexistent__/project".to_string())
}

#[test]
fn test_enter_with_nonexistent_path_enters_confirm() {
    let mut dialog = nonexistent_dialog();
    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(matches!(result, DialogResult::Continue));
    assert_eq!(dialog.confirm_create_dir, Some(false));
}

#[test]
fn test_enter_with_existing_path_submits_directly() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut dialog =
        NewSessionDialog::new_with_tools(vec!["claude"], tmp.path().to_string_lossy().to_string());
    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(matches!(result, DialogResult::Submit(_)));
    assert!(dialog.confirm_create_dir.is_none());
}

#[test]
fn test_confirm_esc_cancels() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(false);
    let result = dialog.handle_key(key(KeyCode::Esc));
    assert!(matches!(result, DialogResult::Continue));
    assert!(dialog.confirm_create_dir.is_none());
    assert_eq!(dialog.focused_field, dialog.path_field());
}

#[test]
fn test_confirm_n_cancels() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(true);
    dialog.handle_key(key(KeyCode::Char('n')));
    assert!(dialog.confirm_create_dir.is_none());
    assert_eq!(dialog.focused_field, dialog.path_field());
}

#[test]
fn test_confirm_h_selects_yes() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(false);
    dialog.handle_key(key(KeyCode::Char('h')));
    assert_eq!(dialog.confirm_create_dir, Some(true));
}

#[test]
fn test_confirm_l_selects_no() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(true);
    dialog.handle_key(key(KeyCode::Char('l')));
    assert_eq!(dialog.confirm_create_dir, Some(false));
}

#[test]
fn test_confirm_tab_toggles() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(false);
    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.confirm_create_dir, Some(true));
    dialog.confirm_create_dir = Some(true);
    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.confirm_create_dir, Some(false));
}

#[test]
fn test_confirm_y_creates_dir_and_submits() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let new_path = tmp.path().join("new_project");
    assert!(!new_path.exists());

    let mut dialog =
        NewSessionDialog::new_with_tools(vec!["claude"], new_path.to_string_lossy().to_string());
    dialog.confirm_create_dir = Some(false);
    let result = dialog.handle_key(key(KeyCode::Char('y')));
    assert!(matches!(result, DialogResult::Submit(_)));
    assert!(new_path.exists());
}

#[test]
fn test_confirm_enter_yes_creates_dir_and_submits() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let new_path = tmp.path().join("another_dir");

    let mut dialog =
        NewSessionDialog::new_with_tools(vec!["claude"], new_path.to_string_lossy().to_string());
    dialog.confirm_create_dir = Some(true);
    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(matches!(result, DialogResult::Submit(_)));
    assert!(new_path.exists());
}

#[test]
fn test_confirm_enter_no_cancels() {
    let mut dialog = nonexistent_dialog();
    dialog.confirm_create_dir = Some(false);
    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(matches!(result, DialogResult::Continue));
    assert!(dialog.confirm_create_dir.is_none());
    assert_eq!(dialog.focused_field, dialog.path_field());
}

#[test]
fn test_confirm_create_failure_shows_error() {
    let mut dialog = NewSessionDialog::new_with_tools(
        vec!["claude"],
        "/proc/aoe_test_cannot_create".to_string(),
    );
    dialog.confirm_create_dir = Some(true);
    let result = dialog.handle_key(key(KeyCode::Char('y')));
    assert!(matches!(result, DialogResult::Continue));
    assert!(dialog.error_message.is_some());
    assert!(dialog.confirm_create_dir.is_none());
}

// --- Profile picker tests ---

#[test]
fn test_profile_cycling() {
    let mut dialog = single_tool_dialog();
    dialog.available_profiles = vec![
        "default".to_string(),
        "work".to_string(),
        "personal".to_string(),
    ];
    dialog.profile_descriptions = vec![None, None, None];
    dialog.profile_index = 0;
    dialog.focused_field = 0; // profile field

    // Right cycles forward
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.selected_profile(), "work");

    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.selected_profile(), "personal");

    // Wraps around
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.selected_profile(), "default");

    // Left cycles backward
    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.selected_profile(), "personal");
}

#[test]
fn test_profile_single_profile_no_cycle() {
    let mut dialog = single_tool_dialog();
    dialog.available_profiles = vec!["default".to_string()];
    dialog.profile_descriptions = vec![None];
    dialog.profile_index = 0;
    dialog.focused_field = 0;

    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.selected_profile(), "default");
    assert_eq!(dialog.profile_index, 0);
}

#[test]
fn test_profile_included_in_submit() {
    let mut dialog = single_tool_dialog();
    dialog.available_profiles = vec!["default".to_string(), "work".to_string()];
    dialog.profile_descriptions = vec![None, None];
    dialog.focused_field = 0;

    dialog.handle_key(key(KeyCode::Right)); // switch to "work"
    let result = dialog.handle_key(key(KeyCode::Enter));

    match result {
        DialogResult::Submit(data) => {
            assert_eq!(data.profile, "work");
        }
        _ => panic!("Expected Submit"),
    }
}

#[test]
#[serial_test::serial]
fn test_profile_switch_reloads_sandbox_env_without_session_override() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let old_home = std::env::var_os("HOME");
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("HOME", temp_home.path());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));
    std::env::set_var("OLD_THING", "1");
    std::env::set_var("NEW_THING", "2");

    let app_dir = crate::session::get_app_dir().expect("app dir");
    let profiles_dir = app_dir.join("profiles");
    fs::create_dir_all(profiles_dir.join("default")).expect("default profile");
    fs::create_dir_all(profiles_dir.join("aoe")).expect("aoe profile");
    fs::write(
        app_dir.join("config.toml"),
        r#"
default_profile = "default"

[sandbox]
enabled_by_default = true
environment = ["THING=$OLD_THING"]
"#,
    )
    .expect("global config");
    fs::write(
        profiles_dir.join("aoe").join("config.toml"),
        r#"
[sandbox]
environment = ["THING=$NEW_THING"]
"#,
    )
    .expect("profile config");

    let mut dialog = single_tool_dialog();
    dialog.available_profiles = vec!["default".to_string(), "aoe".to_string()];
    dialog.profile_descriptions = vec![None, None];
    dialog.profile_index = 0;
    dialog.docker_available = true;
    dialog.reload_config_defaults();
    assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);

    dialog.focused_field = 0;
    dialog.handle_key(key(KeyCode::Right));

    assert_eq!(dialog.selected_profile(), "aoe");
    assert!(dialog.sandbox_enabled);
    assert_eq!(dialog.extra_env, vec!["THING=$NEW_THING".to_string()]);
    assert!(!dialog.extra_env_overridden);

    match dialog.build_submit_result() {
        DialogResult::Submit(data) => {
            assert_eq!(data.profile, "aoe");
            assert!(
                data.extra_env.is_empty(),
                "inherited sandbox env must not become a session override"
            );
        }
        _ => panic!("Expected Submit"),
    }

    std::env::remove_var("OLD_THING");
    std::env::remove_var("NEW_THING");
    match old_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match old_xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
}

#[test]
#[serial_test::serial]
fn test_set_path_reloads_repo_sandbox_env_without_session_override() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let old_home = std::env::var_os("HOME");
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("HOME", temp_home.path());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

    let app_dir = crate::session::get_app_dir().expect("app dir");
    let profiles_dir = app_dir.join("profiles");
    fs::create_dir_all(profiles_dir.join("default")).expect("default profile");
    fs::write(
        app_dir.join("config.toml"),
        r#"
default_profile = "default"

[sandbox]
enabled_by_default = true
environment = ["THING=$OLD_THING"]
"#,
    )
    .expect("global config");

    let repo = tempfile::tempdir().expect("repo dir");
    fs::create_dir_all(repo.path().join(".agent-of-empires")).expect("repo config dir");
    fs::write(
        repo.path().join(".agent-of-empires/config.toml"),
        r#"
[sandbox]
environment = ["THING=$REPO_THING"]
"#,
    )
    .expect("repo config");

    let mut dialog = single_tool_dialog();
    dialog.docker_available = true;
    dialog.reload_config_defaults();
    assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);

    dialog.set_path(repo.path().to_string_lossy().to_string());

    assert_eq!(dialog.extra_env, vec!["THING=$REPO_THING".to_string()]);
    assert!(!dialog.extra_env_overridden);
    match dialog.build_submit_result() {
        DialogResult::Submit(data) => {
            assert!(
                data.extra_env.is_empty(),
                "repo-inherited sandbox env must not become a session override"
            );
        }
        _ => panic!("Expected Submit"),
    }

    match old_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match old_xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
}

#[test]
#[serial_test::serial]
fn test_sandbox_config_refreshes_typed_path_repo_env_without_session_override() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let old_home = std::env::var_os("HOME");
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("HOME", temp_home.path());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

    let app_dir = crate::session::get_app_dir().expect("app dir");
    let profiles_dir = app_dir.join("profiles");
    fs::create_dir_all(profiles_dir.join("default")).expect("default profile");
    fs::write(
        app_dir.join("config.toml"),
        r#"
default_profile = "default"

[sandbox]
enabled_by_default = true
environment = ["THING=$OLD_THING"]
"#,
    )
    .expect("global config");

    let repo = tempfile::tempdir().expect("repo dir");
    fs::create_dir_all(repo.path().join(".agent-of-empires")).expect("repo config dir");
    fs::write(
        repo.path().join(".agent-of-empires/config.toml"),
        r#"
[sandbox]
environment = ["THING=$REPO_THING"]
"#,
    )
    .expect("repo config");

    let mut dialog = single_tool_dialog();
    dialog.docker_available = true;
    dialog.reload_config_defaults();
    assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);

    dialog.path = Input::new(repo.path().to_string_lossy().to_string());
    dialog.focused_field = 4;
    let result = dialog.handle_key(ctrl_key(KeyCode::Char('p')));

    assert!(matches!(result, DialogResult::Continue));
    assert!(dialog.sandbox_config_mode);
    assert_eq!(dialog.extra_env, vec!["THING=$REPO_THING".to_string()]);
    assert!(!dialog.extra_env_overridden);
    match dialog.build_submit_result() {
        DialogResult::Submit(data) => {
            assert!(
                data.extra_env.is_empty(),
                "repo-inherited sandbox env must not become a session override"
            );
        }
        _ => panic!("Expected Submit"),
    }

    match old_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match old_xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
}

#[test]
fn test_env_list_edit_submits_session_override() {
    let mut dialog = single_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.extra_env = vec!["THING=$NEW_THING".to_string()];
    dialog.env_list_expanded = true;
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 1;

    dialog.handle_env_list_key(key(KeyCode::Char('a')));
    for ch in "EXTRA=1".chars() {
        dialog.handle_env_list_key(key(KeyCode::Char(ch)));
    }
    dialog.handle_env_list_key(key(KeyCode::Enter));

    assert!(dialog.extra_env_overridden);
    match dialog.build_submit_result() {
        DialogResult::Submit(data) => {
            assert_eq!(
                data.extra_env,
                vec!["THING=$NEW_THING".to_string(), "EXTRA=1".to_string()]
            );
        }
        _ => panic!("Expected Submit"),
    }
}

// --- Sandbox config mode tests ---

#[test]
fn test_ctrl_p_on_sandbox_enters_config_mode() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    // sandbox field (single profile): title=0, path=1, tool=2, yolo=3, worktree=4, sandbox=5
    dialog.focused_field = 5;

    let result = dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(matches!(result, DialogResult::Continue));
    assert!(dialog.sandbox_config_mode);
    assert_eq!(dialog.sandbox_focused_field, 0);
}

#[test]
fn test_enter_on_sandbox_submits() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.focused_field = 6; // sandbox field

    let result = dialog.handle_key(key(KeyCode::Enter));
    // Enter should submit, not enter config mode
    assert!(!dialog.sandbox_config_mode);
    assert!(matches!(result, DialogResult::Submit(_)));
}

#[test]
fn test_ctrl_p_on_disabled_sandbox_does_not_open_config() {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = false;
    dialog.focused_field = 6; // sandbox field

    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(!dialog.sandbox_config_mode);
}

#[test]
fn test_sandbox_config_mode_esc_returns_to_main() {
    let mut dialog = multi_tool_dialog();
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 1;

    let result = dialog.handle_key(key(KeyCode::Esc));
    assert!(matches!(result, DialogResult::Continue));
    assert!(!dialog.sandbox_config_mode);
}

#[test]
fn test_sandbox_config_mode_tab_cycles() {
    let mut dialog = multi_tool_dialog();
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 0;

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.sandbox_focused_field, 1);

    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.sandbox_focused_field, 0); // wrap
}

#[test]
fn test_sandbox_config_mode_enter_on_image_returns_to_main() {
    let mut dialog = multi_tool_dialog();
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 0; // image

    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(matches!(result, DialogResult::Continue));
    assert!(!dialog.sandbox_config_mode);
}

#[test]
fn test_scratch_toggle_with_ctrl_t() {
    let mut dialog = single_tool_dialog();
    assert!(!dialog.scratch);

    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(dialog.scratch, "Ctrl+T must turn scratch on");

    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(!dialog.scratch, "Ctrl+T again must turn it off");
}

#[test]
fn test_scratch_toggle_clears_worktree() {
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;

    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(dialog.scratch);
    assert!(
        !dialog.worktree_enabled,
        "enabling scratch must clear the worktree toggle"
    );
}

#[test]
fn test_scratch_submit_sends_empty_path_and_no_worktree() {
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    let result = dialog.handle_key(key(KeyCode::Enter));
    match result {
        DialogResult::Submit(data) => {
            assert!(data.scratch, "data.scratch must be true");
            assert_eq!(data.path, "", "scratch submit must send an empty path");
            assert!(
                !data.worktree_enabled,
                "scratch submit must force worktree_enabled off"
            );
            assert!(
                data.worktree_branch.is_none(),
                "scratch submit must not carry a worktree branch"
            );
        }
        other => panic!("Expected Submit, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn test_scratch_skips_path_existence_confirmation() {
    let mut dialog = single_tool_dialog();
    // Use a nonexistent path; without scratch, Enter would open the
    // "Create dir?" confirmation.
    dialog.path = Input::new("/does/not/exist/scratch-test".to_string());
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    let result = dialog.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(result, DialogResult::Submit(_)),
        "scratch must skip the path-exists check on Enter"
    );
    assert!(
        dialog.confirm_create_dir.is_none(),
        "create-dir confirmation must not activate for scratch sessions"
    );
}

#[test]
fn test_worktree_toggle_blocked_when_scratch_on() {
    // Defense against the UI path that would otherwise let the user submit
    // scratch + worktree_branch and trip the server-side 400. With scratch
    // on, Space on the worktree row must be a no-op (with a user-facing
    // error_message) rather than silently re-enabling worktree.
    let mut dialog = single_tool_dialog();
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(dialog.scratch);
    assert!(!dialog.worktree_enabled);

    // Single-tool, no-profile layout: path=0, title=1, yolo=2, worktree=3.
    dialog.focused_field = 3;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(
        !dialog.worktree_enabled,
        "worktree toggle must be blocked while scratch is on"
    );
    assert!(
        dialog.error_message.is_some(),
        "user must see an explanation, not a silent no-op"
    );
}

/// Stage a focusable rect at a known position so click / hover routing
/// can be tested without going through a full render pass.
fn stage_focusable(dialog: &mut NewSessionDialog, field: usize, rect: ratatui::layout::Rect) {
    dialog.focusable_rects.push((field, rect));
}

#[test]
fn click_on_unstaged_dialog_returns_none() {
    let mut dialog = single_tool_dialog();
    assert!(dialog.handle_click(5, 5).is_none());
}

#[test]
fn click_on_yolo_row_toggles_yolo() {
    let mut dialog = single_tool_dialog();
    // Single-tool, no-profile layout: path=0, title=1, yolo=2.
    stage_focusable(&mut dialog, 2, ratatui::layout::Rect::new(0, 5, 30, 1));
    let before = dialog.yolo_mode;
    let result = dialog.handle_click(10, 5).expect("click should hit yolo");
    assert!(matches!(result, DialogResult::Continue));
    assert_eq!(dialog.focused_field, 2);
    assert_eq!(dialog.yolo_mode, !before);
}

#[test]
fn click_on_path_field_just_moves_focus() {
    let mut dialog = single_tool_dialog();
    stage_focusable(&mut dialog, 0, ratatui::layout::Rect::new(0, 3, 30, 1));
    dialog.focused_field = 1; // start elsewhere
    let path_before = dialog.path.value().to_string();
    let result = dialog.handle_click(10, 3).expect("click should hit path");
    assert!(matches!(result, DialogResult::Continue));
    assert_eq!(dialog.focused_field, 0);
    assert_eq!(
        dialog.path.value(),
        path_before,
        "clicking the path row must not edit the path"
    );
}

#[test]
fn click_on_tool_cycles_when_multi_tool() {
    let mut dialog = multi_tool_dialog();
    // Multi-tool, no-profile layout: path=0, title=1, tool=2.
    stage_focusable(&mut dialog, 2, ratatui::layout::Rect::new(0, 5, 30, 1));
    let before = dialog.tool_index;
    dialog.handle_click(10, 5).expect("click should hit tool");
    assert_eq!(
        dialog.tool_index,
        (before + 1) % dialog.available_tools.len(),
        "clicking the tool row should cycle to the next tool"
    );
    assert_eq!(dialog.focused_field, 2);
}

#[test]
fn hover_over_field_does_not_move_focus() {
    // Hover must never steal focus from the field the user is editing.
    // Otherwise a stray mouse drift while typing the title moves focus
    // to whatever row the cursor crossed, and the next keystroke goes
    // somewhere unexpected.
    let mut dialog = single_tool_dialog();
    stage_focusable(&mut dialog, 2, ratatui::layout::Rect::new(0, 5, 30, 1));
    let yolo_before = dialog.yolo_mode;
    let focus_before = dialog.focused_field;
    assert_ne!(focus_before, 2);
    let changed = dialog.handle_hover(10, 5);
    assert!(!changed, "hover must not report a focus change");
    assert_eq!(
        dialog.focused_field, focus_before,
        "hover must leave focus alone"
    );
    assert_eq!(
        dialog.yolo_mode, yolo_before,
        "hover must not toggle anything either"
    );
}

#[test]
fn hover_off_focusables_does_not_change_focus() {
    let mut dialog = single_tool_dialog();
    stage_focusable(&mut dialog, 0, ratatui::layout::Rect::new(0, 3, 30, 1));
    dialog.focused_field = 1;
    assert!(!dialog.handle_hover(80, 80));
    assert_eq!(dialog.focused_field, 1);
}

#[test]
fn click_on_worktree_while_scratch_on_surfaces_error_not_toggle() {
    let mut dialog = single_tool_dialog();
    // Turn scratch on; mirror the keyboard behavior; worktree click
    // while scratch is on must NOT silently re-enable worktree.
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(dialog.scratch);
    assert!(!dialog.worktree_enabled);
    // Single-tool layout: path=0, title=1, yolo=2, worktree=3.
    stage_focusable(&mut dialog, 3, ratatui::layout::Rect::new(0, 7, 30, 1));
    dialog.error_message = None;
    dialog.handle_click(10, 7);
    assert!(
        !dialog.worktree_enabled,
        "worktree toggle must be blocked while scratch is on"
    );
    assert!(dialog.error_message.is_some());
}

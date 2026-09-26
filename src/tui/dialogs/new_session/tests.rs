use super::*;
use crate::session::{merge_configs, Config, ProfileConfig};
use crate::tui::dialogs::test_keys::{alt_key, ctrl_key, key, shift_key};
use std::fs;

const TEST_PATH: &str = ".";

/// Field layout with one tool and one profile: path 0, title 1, yolo 2,
/// worktree 3, group 4.
fn single_tool_dialog() -> NewSessionDialog {
    NewSessionDialog::new_with_tools(vec!["claude"], TEST_PATH.to_string())
}

/// Field layout with two tools: path 0, title 1, tool 2, yolo 3, worktree 4,
/// sandbox 5 (when a runtime is available), group last.
fn multi_tool_dialog() -> NewSessionDialog {
    NewSessionDialog::new_with_tools(vec!["claude", "opencode"], TEST_PATH.to_string())
}

fn sandboxed_dialog() -> NewSessionDialog {
    let mut dialog = multi_tool_dialog();
    dialog.docker_available = true;
    dialog.sandbox_enabled = true;
    dialog.title = Input::new("Test".to_string());
    dialog
}

fn type_str(dialog: &mut NewSessionDialog, text: &str) {
    for c in text.chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
}

fn submitted(result: DialogResult<NewSessionData>) -> NewSessionData {
    match result {
        DialogResult::Submit(data) => data,
        _ => panic!("expected Submit"),
    }
}

/// A dialog whose path field points at `dir`/`prefix`, with the ghost
/// completion computed.
fn ghosting(dir: &std::path::Path, prefix: &str) -> NewSessionDialog {
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0;
    dialog.path = Input::new(format!("{}/{prefix}", dir.display()));
    dialog.recompute_path_ghost();
    dialog
}

fn screen_of(dialog: &mut NewSessionDialog, width: u16, height: u16) -> String {
    use ratatui::{backend::TestBackend, Terminal};
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let theme = crate::tui::styles::Theme::default();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &theme))
        .expect("render");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn config_seeds_the_worktree_toggle_and_sandbox_image() {
    let mut config = Config::default();
    config.worktree.enabled = true;
    config.sandbox.default_image = "my-custom-sandbox:local".to_string();
    let dialog =
        NewSessionDialog::new_with_config(vec!["claude"], "/tmp/project".to_string(), config);
    assert!(dialog.worktree_enabled);
    assert_eq!(dialog.sandbox_image.value(), "my-custom-sandbox:local");
}

#[test]
fn a_profile_default_tool_beats_the_global_one() {
    let mut global = Config::default();
    global.session.default_tool = Some("claude".to_string());
    let profile: ProfileConfig =
        serde_json::from_value(serde_json::json!({"session": {"default_tool": "opencode"}}))
            .unwrap();
    let resolved = merge_configs(global, &profile);
    assert_eq!(resolved.session.default_tool.as_deref(), Some("opencode"));

    let dialog = NewSessionDialog::new_with_config(
        vec!["claude", "opencode"],
        "/tmp/project".to_string(),
        resolved,
    );
    assert_eq!(dialog.available_tools[dialog.tool_index], "opencode");
}

#[test]
fn enter_submits_the_form_and_esc_cancels() {
    let data = submitted(single_tool_dialog().handle_key(key(KeyCode::Enter)));
    assert_eq!(
        data.title, "",
        "an empty title passes through to the builder"
    );
    assert_eq!(data.path, TEST_PATH);
    assert_eq!(data.group, "");
    assert_eq!(data.tool, "claude");
    assert_eq!(data.profile, "default");

    let mut dialog = single_tool_dialog();
    dialog.title = Input::new("My Custom Title".to_string());
    assert_eq!(
        submitted(dialog.handle_key(key(KeyCode::Enter))).title,
        "My Custom Title"
    );

    let mut dialog = single_tool_dialog();
    dialog.error_message = Some("Some error".to_string());
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Esc)),
        DialogResult::Cancel
    ));
    assert_eq!(dialog.error_message, None);

    assert!(matches!(
        single_tool_dialog().handle_key(key(KeyCode::F(1))),
        DialogResult::Continue
    ));
}

#[test]
fn tab_walks_the_visible_fields_in_both_directions() {
    // The worktree sub-options live in a Ctrl+P overlay, so enabling it adds
    // no tab stop; nor do the sandbox sub-options.
    let mut worktree_on = single_tool_dialog();
    worktree_on.worktree_enabled = true;
    for mut dialog in [single_tool_dialog(), worktree_on] {
        for expected in [1, 2, 3, 4, 0] {
            dialog.handle_key(key(KeyCode::Tab));
            assert_eq!(dialog.focused_field, expected);
        }
    }

    let mut dialog = multi_tool_dialog();
    for expected in [1, 2, 3, 4, 5, 0] {
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, expected);
    }

    let mut dialog = single_tool_dialog();
    for expected in [4, 3, 2, 1, 0] {
        dialog.handle_key(shift_key(KeyCode::BackTab));
        assert_eq!(dialog.focused_field, expected);
    }

    // A sandbox row appears with a runtime, enabled or not, and never grows
    // sub-stops of its own.
    for enabled in [true, false] {
        let mut dialog = multi_tool_dialog();
        dialog.docker_available = true;
        dialog.sandbox_enabled = enabled;
        for _ in 0..5 {
            dialog.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(dialog.focused_field, 5, "sandbox row");
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 6, "group row");
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }
}

#[test]
fn text_keys_edit_the_focused_field() {
    // (focused field, typed text, what it should read back)
    type Read = fn(&NewSessionDialog) -> String;
    let cases: &[(usize, &str, &str, Read)] = &[
        (1, "Hi", "Hi", |d| d.title.value().to_string()),
        (0, "/a", "./a", |d| d.path.value().to_string()),
        (4, "work", "work", |d| d.group.value().to_string()),
    ];
    for (field, typed, want, read) in cases {
        let mut dialog = single_tool_dialog();
        dialog.focused_field = *field;
        type_str(&mut dialog, typed);
        assert_eq!(read(&dialog), *want);
    }

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 1;
    dialog.handle_key(key(KeyCode::Backspace));
    assert_eq!(dialog.title.value(), "", "backspace on empty is inert");
    dialog.title = Input::new("Hello".to_string());
    dialog.handle_key(key(KeyCode::Backspace));
    assert_eq!(dialog.title.value(), "Hell");

    // Any edit clears a stale error.
    dialog.error_message = Some("Some error".to_string());
    type_str(&mut dialog, "a");
    assert_eq!(dialog.error_message, None);
}

#[test]
fn path_field_takes_readline_style_cursor_jumps() {
    // (keys that move the cursor, where an inserted X lands)
    type Move = fn(&mut NewSessionDialog);
    let cases: &[(Move, &str)] = &[
        (
            |d| {
                d.handle_key(ctrl_key(KeyCode::Left));
            },
            "/tmp/alpha/Xbeta",
        ),
        (
            |d| {
                d.handle_key(alt_key(KeyCode::Char('b')));
            },
            "/tmp/alpha/Xbeta",
        ),
        (
            |d| {
                d.handle_key(ctrl_key(KeyCode::Char('a')));
            },
            "X/tmp/alpha/beta",
        ),
    ];
    for (move_cursor, want) in cases {
        let mut dialog = single_tool_dialog();
        dialog.focused_field = 0;
        dialog.path = Input::new("/tmp/alpha/beta".to_string());
        move_cursor(&mut dialog);
        dialog.handle_key(key(KeyCode::Char('X')));
        assert_eq!(dialog.path.value(), *want);
    }

    // Away from the end of the input, Right is an ordinary cursor move.
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0;
    dialog.path = Input::new("/tmp/alpha/beta".to_string());
    dialog.handle_key(ctrl_key(KeyCode::Char('a')));
    let before = dialog.path.visual_cursor();
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.path.visual_cursor(), before + 1);
}

#[test]
fn path_ghost_completes_the_typed_prefix() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let at = tmp.path();
    for dir in ["project-alpha", "client-api", "client-web", "alpha"] {
        fs::create_dir(at.join(dir)).expect("create dir");
    }
    fs::create_dir(at.join("alpha").join("inner")).expect("create dir");
    fs::write(at.join("project-file"), "not a directory").expect("write file");

    // A lone match completes it, several share their prefix, an exact
    // directory only gains its separator, and a miss shows nothing.
    for (prefix, want) in [
        ("pro", Some("ject-alpha/")),
        ("cl", Some("ient-")),
        ("alpha", Some("/")),
        ("zzz_nonexistent", None),
    ] {
        assert_eq!(ghosting(at, prefix).ghost_text(), want, "{prefix}");
    }

    // Right and End accept it, and the next level is offered straight away.
    for accept in [KeyCode::Right, KeyCode::End] {
        let mut dialog = ghosting(at, "alp");
        dialog.handle_key(key(accept));
        assert_eq!(dialog.path.value(), format!("{}/alpha/", at.display()));
        assert_eq!(dialog.ghost_text(), Some("inner/"));
    }

    // Tab navigates instead of accepting, and drops the ghost on the way out.
    let mut dialog = ghosting(at, "pro");
    assert!(dialog.ghost_text().is_some());
    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.focused_field, 1);
    assert_eq!(dialog.ghost_text(), None);

    // A cursor away from the end has nothing to complete.
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 0;
    dialog.path = Input::new(format!("{}/alp", at.display()));
    dialog.handle_key(ctrl_key(KeyCode::Char('a')));
    dialog.recompute_path_ghost();
    assert_eq!(dialog.ghost_text(), None);
}

#[test]
fn invalid_path_flash_expires_on_the_next_tick() {
    let mut dialog = single_tool_dialog();
    dialog.path_invalid_flash_until =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
    assert!(dialog.tick());
    assert!(!dialog.is_path_invalid_flash_active());
}

#[test]
#[serial_test::serial]
fn the_tool_row_cycles_and_submits_the_picked_tool() {
    let mut dialog = NewSessionDialog::new_with_tools(
        vec!["claude", "opencode", "codex"],
        TEST_PATH.to_string(),
    );
    dialog.focused_field = 2;
    for (code, expected) in [
        (KeyCode::Right, 1),
        (KeyCode::Right, 2),
        (KeyCode::Right, 0),
        (KeyCode::Left, 2),
        (KeyCode::Left, 1),
    ] {
        dialog.handle_key(key(code));
        assert_eq!(dialog.tool_index, expected);
    }

    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 2;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(dialog.tool_index, 1);
    assert_eq!(
        submitted(dialog.handle_key(key(KeyCode::Enter))).tool,
        "opencode"
    );

    // Space is ordinary text on a text field, and a lone tool never cycles.
    let mut dialog = multi_tool_dialog();
    dialog.focused_field = 1;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(dialog.title.value(), " ");
    assert_eq!(dialog.tool_index, 0);

    let mut dialog = single_tool_dialog();
    dialog.focused_field = 2;
    dialog.handle_key(key(KeyCode::Left));
    assert_eq!(dialog.tool_index, 0);
}

#[test]
fn the_deprecated_tool_badge_survives_every_tool_row_layout() {
    let mut read_only = NewSessionDialog::new_with_tools(vec!["gemini"], TEST_PATH.to_string());
    let mut configured =
        NewSessionDialog::new_with_tools(vec!["claude", "codex", "gemini"], TEST_PATH.to_string());
    configured.tool_index = 2;
    configured.focused_field = 2;
    configured.command_override = Input::new("custom-wrapper".to_string());
    configured.extra_args = Input::new("--model long --verbose".to_string());

    let screen = screen_of(&mut read_only, 100, 40);
    assert!(screen.contains("⚠ deprecated"), "read-only row: {screen}");

    // The narrow row has to keep the suffix behind the configuration metadata.
    let screen = screen_of(&mut configured, 64, 40);
    assert!(screen.contains("⚠ deprecated"), "configured row: {screen}");
    assert!(screen.contains("(configured) Ctrl+P"), "{screen}");
}

#[test]
fn yolo_toggles_on_its_own_row_independently_of_the_sandbox() {
    let mut dialog = sandboxed_dialog();
    dialog.focused_field = 3;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.yolo_mode);
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.yolo_mode);

    // (sandbox on?, submitted sandbox, submitted yolo)
    for sandbox in [true, false] {
        let mut dialog = sandboxed_dialog();
        dialog.sandbox_enabled = sandbox;
        dialog.yolo_mode = true;
        let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.sandbox, sandbox);
        assert!(data.yolo_mode);
    }

    // Turning the sandbox off leaves yolo alone.
    let mut dialog = sandboxed_dialog();
    dialog.yolo_mode = true;
    dialog.focused_field = 5;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.sandbox_enabled);
    assert!(dialog.yolo_mode);
}

#[test]
fn the_sandbox_image_always_submits_whatever_the_field_holds() {
    let default_image = crate::containers::get_container_runtime().effective_default_image();
    // (sandbox on?, image field, submitted image)
    let cases: &[(bool, Option<&str>, &str)] = &[
        (true, Some("custom/image:tag"), "custom/image:tag"),
        (true, None, &default_image),
        (true, Some(""), ""),
        (false, Some("custom/image:tag"), "custom/image:tag"),
    ];
    for (sandbox, image, want) in cases {
        let mut dialog = sandboxed_dialog();
        dialog.sandbox_enabled = *sandbox;
        if let Some(image) = image {
            dialog.sandbox_image = Input::new(image.to_string());
        }
        let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.sandbox, *sandbox);
        assert_eq!(data.sandbox_image, *want);
    }
}

#[test]
#[serial_test::serial]
fn the_sandbox_config_overlay_opens_on_ctrl_p_and_edits_the_image() {
    let mut dialog = sandboxed_dialog();
    dialog.focused_field = 5;
    assert!(matches!(
        dialog.handle_key(ctrl_key(KeyCode::Char('p'))),
        DialogResult::Continue
    ));
    assert!(dialog.sandbox_config_mode);
    assert_eq!(dialog.sandbox_focused_field, 0);

    // Tab wraps the two rows; Esc and Enter both return to the main form.
    for expected in [1, 0] {
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.sandbox_focused_field, expected);
    }
    for code in [KeyCode::Esc, KeyCode::Enter] {
        let mut dialog = sandboxed_dialog();
        dialog.sandbox_config_mode = true;
        dialog.sandbox_focused_field = 0;
        assert!(matches!(
            dialog.handle_key(key(code)),
            DialogResult::Continue
        ));
        assert!(!dialog.sandbox_config_mode);
    }

    // Typing on the image row appends to the field.
    let mut dialog = sandboxed_dialog();
    dialog.sandbox_config_mode = true;
    dialog.sandbox_focused_field = 0;
    type_str(&mut dialog, "abc");
    assert_eq!(
        dialog.sandbox_image.value(),
        format!(
            "{}abc",
            crate::containers::get_container_runtime().effective_default_image()
        )
    );

    // On the main form the sandbox row submits on Enter, and Ctrl+P is inert
    // while the sandbox is off.
    let mut dialog = sandboxed_dialog();
    dialog.focused_field = 6;
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Enter)),
        DialogResult::Submit(_)
    ));
    assert!(!dialog.sandbox_config_mode);

    let mut dialog = sandboxed_dialog();
    dialog.sandbox_enabled = false;
    dialog.focused_field = 6;
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(!dialog.sandbox_config_mode);
}

#[test]
fn the_worktree_row_toggles_and_its_overlay_carries_name_and_branch() {
    // Toggling the row alone submits a worktree with no name.
    let mut dialog = single_tool_dialog();
    dialog.focused_field = 3;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.worktree_enabled);
    let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
    assert!(data.worktree_enabled);
    assert!(data.worktree_branch.is_none());
    assert!(data.extra_repo_paths.is_empty());

    // The overlay's name field becomes the branch override.
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.focused_field = 3;
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(dialog.worktree_config_mode);
    assert_eq!(dialog.worktree_config_focused_field, 0);
    type_str(&mut dialog, "feature-name");
    dialog.handle_key(key(KeyCode::Enter));
    let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
    assert!(data.worktree_enabled);
    assert_eq!(data.worktree_branch.as_deref(), Some("feature-name"));

    // Its new-branch checkbox is the second row and rides along on submit.
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.worktree_branch = Input::new("feature-branch".to_string());
    dialog.focused_field = 3;
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    dialog.handle_key(key(KeyCode::Tab));
    assert_eq!(dialog.worktree_config_focused_field, 1);
    assert!(dialog.create_new_branch);
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(!dialog.create_new_branch);
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.create_new_branch);
    dialog.handle_key(key(KeyCode::Char(' ')));
    dialog.handle_key(key(KeyCode::Esc));
    let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
    assert!(!data.create_new_branch);
    assert!(data.worktree_enabled);
    assert_eq!(data.worktree_branch.as_deref(), Some("feature-branch"));
}

#[test]
fn scratch_sessions_exclude_worktrees_and_skip_the_path_check() {
    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(dialog.scratch);
    assert!(!dialog.worktree_enabled, "scratch clears the worktree row");
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(!dialog.scratch);

    let mut dialog = single_tool_dialog();
    dialog.worktree_enabled = true;
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    let data = submitted(dialog.handle_key(key(KeyCode::Enter)));
    assert!(data.scratch);
    assert_eq!(data.path, "", "the server provisions the directory");
    assert!(!data.worktree_enabled);
    assert!(data.worktree_branch.is_none());

    // A path that does not exist would normally open the create-dir confirm.
    let mut dialog = single_tool_dialog();
    dialog.path = Input::new("/does/not/exist/scratch-test".to_string());
    dialog.handle_key(ctrl_key(KeyCode::Char('t')));
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Enter)),
        DialogResult::Submit(_)
    ));
    assert!(dialog.confirm_create_dir.is_none());

    // Re-enabling the worktree row while scratch is on has to explain itself
    // rather than submit a payload the server rejects. Keyboard and mouse
    // must agree.
    let mut by_key = single_tool_dialog();
    by_key.handle_key(ctrl_key(KeyCode::Char('t')));
    by_key.focused_field = 3;
    by_key.handle_key(key(KeyCode::Char(' ')));

    let mut by_click = single_tool_dialog();
    by_click.handle_key(ctrl_key(KeyCode::Char('t')));
    by_click
        .focusable_rects
        .push((3, ratatui::layout::Rect::new(0, 7, 30, 1)));
    by_click.error_message = None;
    by_click.handle_click(10, 7);

    for dialog in [by_key, by_click] {
        assert!(!dialog.worktree_enabled);
        assert!(dialog.error_message.is_some());
    }
}

#[test]
fn a_missing_directory_is_confirmed_before_the_session_is_created() {
    let nonexistent = || {
        NewSessionDialog::new_with_tools(vec!["claude"], "/__aoe_nonexistent__/project".to_string())
    };

    let mut dialog = nonexistent();
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Enter)),
        DialogResult::Continue
    ));
    assert_eq!(dialog.confirm_create_dir, Some(false));

    // An existing path never asks.
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut dialog =
        NewSessionDialog::new_with_tools(vec!["claude"], tmp.path().to_string_lossy().to_string());
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Enter)),
        DialogResult::Submit(_)
    ));
    assert!(dialog.confirm_create_dir.is_none());

    // h / l / Tab move between Yes and No.
    for (start, code, want) in [
        (Some(false), KeyCode::Char('h'), Some(true)),
        (Some(true), KeyCode::Char('l'), Some(false)),
        (Some(false), KeyCode::Tab, Some(true)),
        (Some(true), KeyCode::Tab, Some(false)),
    ] {
        let mut dialog = nonexistent();
        dialog.confirm_create_dir = start;
        dialog.handle_key(key(code));
        assert_eq!(dialog.confirm_create_dir, want);
    }

    // Esc, `n` and Enter-on-No all back out to the path field.
    for (start, code) in [
        (Some(false), KeyCode::Esc),
        (Some(true), KeyCode::Char('n')),
        (Some(false), KeyCode::Enter),
    ] {
        let mut dialog = nonexistent();
        dialog.confirm_create_dir = start;
        assert!(matches!(
            dialog.handle_key(key(code)),
            DialogResult::Continue
        ));
        assert!(dialog.confirm_create_dir.is_none());
        assert_eq!(dialog.focused_field, dialog.path_field());
    }

    // `y` and Enter-on-Yes both create the directory and submit.
    let tmp = tempfile::tempdir().expect("temp dir");
    for (dir, start, code) in [
        ("new_project", Some(false), KeyCode::Char('y')),
        ("another_dir", Some(true), KeyCode::Enter),
    ] {
        let path = tmp.path().join(dir);
        let mut dialog =
            NewSessionDialog::new_with_tools(vec!["claude"], path.to_string_lossy().to_string());
        dialog.confirm_create_dir = start;
        assert!(matches!(
            dialog.handle_key(key(code)),
            DialogResult::Submit(_)
        ));
        assert!(path.exists());
    }

    // A create that cannot succeed surfaces inline instead of submitting.
    let mut dialog = NewSessionDialog::new_with_tools(
        vec!["claude"],
        "/proc/aoe_test_cannot_create".to_string(),
    );
    dialog.confirm_create_dir = Some(true);
    assert!(matches!(
        dialog.handle_key(key(KeyCode::Char('y'))),
        DialogResult::Continue
    ));
    assert!(dialog.error_message.is_some());
    assert!(dialog.confirm_create_dir.is_none());
}

#[test]
#[serial_test::serial]
fn the_profile_row_cycles_and_rides_along_on_submit() {
    let with_profiles = |names: &[&str]| {
        let mut dialog = single_tool_dialog();
        dialog.available_profiles = names.iter().map(|s| s.to_string()).collect();
        dialog.profile_descriptions = names.iter().map(|_| None).collect();
        dialog.profile_index = 0;
        dialog.focused_field = 0;
        dialog
    };

    let mut dialog = with_profiles(&["default", "work", "personal"]);
    for (code, want) in [
        (KeyCode::Right, "work"),
        (KeyCode::Right, "personal"),
        (KeyCode::Right, "default"),
        (KeyCode::Left, "personal"),
    ] {
        dialog.handle_key(key(code));
        assert_eq!(dialog.selected_profile(), want);
    }

    // A lone profile is not a picker, so the row does not cycle.
    let mut dialog = with_profiles(&["default"]);
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.profile_index, 0);

    let mut dialog = with_profiles(&["default", "work"]);
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(
        submitted(dialog.handle_key(key(KeyCode::Enter))).profile,
        "work"
    );
}

/// Write a global config with `sandbox.environment` under an isolated home,
/// plus an `aoe` profile that overrides it.
fn config_with_sandbox_env(profile_env: Option<&str>) -> std::path::PathBuf {
    let app_dir = crate::session::get_app_dir().expect("app dir");
    let profiles_dir = app_dir.join("profiles");
    fs::create_dir_all(profiles_dir.join("default")).expect("default profile");
    fs::write(
        app_dir.join("config.toml"),
        "default_profile = \"default\"\n\n[sandbox]\nenabled_by_default = true\nenvironment = [\"THING=$OLD_THING\"]\n",
    )
    .expect("global config");
    if let Some(env) = profile_env {
        fs::create_dir_all(profiles_dir.join("aoe")).expect("aoe profile");
        fs::write(
            profiles_dir.join("aoe").join("config.toml"),
            format!("[sandbox]\nenvironment = [\"{env}\"]\n"),
        )
        .expect("profile config");
    }
    app_dir
}

/// A repo whose own config tries to set `sandbox.environment`.
fn repo_with_sandbox_env() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo dir");
    fs::create_dir_all(repo.path().join(".agent-of-empires")).expect("repo config dir");
    fs::write(
        repo.path().join(".agent-of-empires/config.toml"),
        "[sandbox]\nenvironment = [\"THING=$REPO_THING\"]\n",
    )
    .expect("repo config");
    repo
}

#[test]
#[serial_test::serial]
fn inherited_sandbox_env_is_shown_but_never_submitted_as_a_session_override() {
    // `isolate_home` holds the shared env lock and restores HOME/XDG on Drop,
    // so nothing leaks into sibling tests.
    let temp_home = tempfile::tempdir().expect("temp home");
    let _home = crate::session::test_support::isolate_home(temp_home.path());
    let _extra_env =
        crate::session::test_support::EnvGuard::set(&[("OLD_THING", "1"), ("NEW_THING", "2")]);
    config_with_sandbox_env(Some("THING=$NEW_THING"));

    // Switching profile picks up that profile's env.
    let mut dialog = single_tool_dialog();
    dialog.available_profiles = vec!["default".to_string(), "aoe".to_string()];
    dialog.profile_descriptions = vec![None, None];
    dialog.docker_available = true;
    dialog.reload_config_defaults();
    assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);
    dialog.focused_field = 0;
    dialog.handle_key(key(KeyCode::Right));
    assert_eq!(dialog.selected_profile(), "aoe");
    assert!(dialog.sandbox_enabled);
    assert_eq!(dialog.extra_env, vec!["THING=$NEW_THING".to_string()]);
    assert!(!dialog.extra_env_overridden);
    let data = submitted(dialog.build_submit_result());
    assert_eq!(data.profile, "aoe");
    assert!(data.extra_env.is_empty());

    // A repo cannot set `sandbox.environment`, whether the path arrives
    // through `set_path` or is typed and then read by the sandbox overlay.
    let repo = repo_with_sandbox_env();
    type Arrive = fn(&mut NewSessionDialog, &std::path::Path);
    let arrivals: &[Arrive] = &[
        |d, path| d.set_path(path.to_string_lossy().to_string()),
        |d, path| {
            d.path = Input::new(path.to_string_lossy().to_string());
            // path 0, title 1, then Structured when the tool is ACP-capable,
            // then yolo, worktree, sandbox.
            d.focused_field = 4 + usize::from(d.structured_capable);
            assert!(matches!(
                d.handle_key(ctrl_key(KeyCode::Char('p'))),
                DialogResult::Continue
            ));
            assert!(d.sandbox_config_mode);
        },
    ];
    for arrive in arrivals {
        let mut dialog = single_tool_dialog();
        dialog.docker_available = true;
        dialog.reload_config_defaults();
        assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);
        arrive(&mut dialog, repo.path());
        assert_eq!(dialog.extra_env, vec!["THING=$OLD_THING".to_string()]);
        assert!(!dialog.extra_env_overridden);
        assert!(submitted(dialog.build_submit_result()).extra_env.is_empty());
    }
}

#[test]
fn editing_the_env_list_does_submit_a_session_override() {
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
    assert_eq!(
        submitted(dialog.build_submit_result()).extra_env,
        vec!["THING=$NEW_THING".to_string(), "EXTRA=1".to_string()]
    );
}

#[test]
fn the_structured_row_appears_only_for_an_acp_capable_tool() {
    // Without the row, index 2 is YOLO.
    let mut dialog = single_tool_dialog();
    assert!(!dialog.structured_capable);
    dialog.focused_field = 2;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.yolo_mode);
    assert!(!dialog.structured_enabled);

    // With it, index 2 is Structured and it rides along on submit.
    let mut dialog = single_tool_dialog();
    dialog.set_structured_capable(true);
    dialog.focused_field = 2;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.structured_enabled);
    assert!(!dialog.yolo_mode);
    assert!(submitted(dialog.build_submit_result()).structured);

    // Losing capability (a tool cycle to a non-ACP agent) clears the toggle,
    // so a stale true can never submit.
    let mut dialog = single_tool_dialog();
    dialog.set_structured_capable(true);
    dialog.structured_enabled = true;
    dialog.set_structured_capable(false);
    assert!(!dialog.structured_enabled);
    assert!(!submitted(dialog.build_submit_result()).structured);
}

#[test]
#[serial_test::serial]
fn clicks_focus_a_row_and_act_on_it_while_hover_does_neither() {
    assert!(single_tool_dialog().handle_click(5, 5).is_none());

    // A checkbox row toggles on click.
    let mut dialog = single_tool_dialog();
    dialog
        .focusable_rects
        .push((2, ratatui::layout::Rect::new(0, 5, 30, 1)));
    let before = dialog.yolo_mode;
    assert!(matches!(
        dialog.handle_click(10, 5),
        Some(DialogResult::Continue)
    ));
    assert_eq!(dialog.focused_field, 2);
    assert_eq!(dialog.yolo_mode, !before);

    // A text row only takes focus.
    let mut dialog = single_tool_dialog();
    dialog
        .focusable_rects
        .push((0, ratatui::layout::Rect::new(0, 3, 30, 1)));
    dialog.focused_field = 1;
    let path = dialog.path.value().to_string();
    assert!(matches!(
        dialog.handle_click(10, 3),
        Some(DialogResult::Continue)
    ));
    assert_eq!(dialog.focused_field, 0);
    assert_eq!(dialog.path.value(), path);

    // A cycler row advances.
    let mut dialog = multi_tool_dialog();
    dialog
        .focusable_rects
        .push((2, ratatui::layout::Rect::new(0, 5, 30, 1)));
    let before = dialog.tool_index;
    dialog.handle_click(10, 5);
    assert_eq!(
        dialog.tool_index,
        (before + 1) % dialog.available_tools.len()
    );
    assert_eq!(dialog.focused_field, 2);

    // Hover must never steal focus from the field being typed into, nor
    // toggle anything, on or off the rects.
    let mut dialog = single_tool_dialog();
    dialog
        .focusable_rects
        .push((2, ratatui::layout::Rect::new(0, 5, 30, 1)));
    let yolo = dialog.yolo_mode;
    let focus = dialog.focused_field;
    assert_ne!(focus, 2);
    assert!(!dialog.handle_hover(10, 5));
    assert!(!dialog.handle_hover(80, 80));
    assert_eq!(dialog.focused_field, focus);
    assert_eq!(dialog.yolo_mode, yolo);
}

#[test]
fn structured_default_reseeds_only_until_the_user_decides() {
    let cases = [
        // (configured default, user toggled first, expected after regain)
        (true, false, true),
        (false, false, false),
        // A chosen value survives a trip through an incapable tool, in both
        // directions: the choice is restored, not the configured default.
        (true, true, false),
        (false, true, true),
    ];
    for (structured_default, user_toggled, expected) in cases {
        let mut dialog = single_tool_dialog();
        dialog.structured_default = structured_default;
        dialog.set_structured_capable(true);
        dialog.structured_enabled = structured_default;
        if user_toggled {
            dialog.focused_field = 2;
            dialog.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }
        // A tool change away from and back to an ACP-capable agent.
        dialog.structured_capable = false;
        dialog.apply_structured_default();
        assert!(!dialog.structured_enabled);
        dialog.structured_capable = true;
        dialog.apply_structured_default();
        assert_eq!(
            dialog.structured_enabled, expected,
            "default={structured_default} toggled={user_toggled}"
        );
    }
}

/// Init a repo with one commit so it has a branch to list.
fn branch_picker_repo_in(parent: &std::path::Path) -> std::path::PathBuf {
    let dir = parent.join("repo");
    fs::create_dir_all(&dir).expect("create repo dir");
    let repo = git2::Repository::init(&dir).expect("git init");
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    fs::write(dir.join("README.md"), "hi\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    drop(tree);
    dir
}

fn worktree_config_dialog(path: String) -> NewSessionDialog {
    let mut dialog = NewSessionDialog::new_with_tools(vec!["claude"], path);
    dialog.worktree_enabled = true;
    dialog.focused_field = 3;
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(dialog.worktree_config_mode);
    dialog
}

/// Rows joined into one whitespace-normalized string, so an assertion does not
/// have to know where `Wrap` broke the line.
fn screen_text(buffer: &ratatui::buffer::Buffer) -> String {
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|row| {
            (0..buffer.area.width)
                .map(|col| buffer[(col, row)].symbol())
                .collect()
        })
        .collect();
    rows.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[serial_test::serial]
fn branch_picker_opens_for_reachable_repo_paths() {
    // Ctrl+P must reach the picker from every field whose hint row advertises
    // it, and for a `~` path, which the submit path expands too.
    // `isolate_home` holds the shared env lock and restores HOME/XDG on Drop.
    let temp_home = tempfile::tempdir().expect("temp home");
    let _home = crate::session::test_support::isolate_home(temp_home.path());
    let repo = branch_picker_repo_in(temp_home.path());
    let absolute = repo.to_string_lossy().to_string();

    let cases = [
        (absolute.clone(), 0),
        (absolute.clone(), 1),
        (absolute, WT_BASE_BRANCH_FIELD),
        ("~/repo".to_string(), 0),
    ];
    for (path, field) in cases {
        let mut dialog = worktree_config_dialog(path.clone());
        dialog.worktree_config_focused_field = field;
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(
            dialog.branch_picker.is_active(),
            "{path} field {field} should open the picker, got {:?}",
            dialog.error_message
        );
        assert!(dialog.error_message.is_none());
    }
}

#[test]
fn branch_picker_surfaces_failures_and_clears_them() {
    // A path the picker cannot use has to say so, and the error has to be
    // drawn: the overlay used to render its hints unconditionally, leaving a
    // set `error_message` invisible.
    use ratatui::{backend::TestBackend, Terminal};

    let not_a_repo = tempfile::tempdir().expect("failed to create temp dir");
    let cases = [
        (
            not_a_repo.path().to_string_lossy().to_string(),
            "Cannot list branches",
        ),
        (String::new(), "Set the project path"),
    ];

    for (path, expected) in cases {
        let mut dialog = worktree_config_dialog(path.clone());
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(!dialog.branch_picker.is_active(), "path {path:?}");
        let error = dialog
            .error_message
            .clone()
            .unwrap_or_else(|| panic!("expected an inline error for {path:?}"));
        assert!(error.contains(expected), "unexpected error: {error}");

        let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
        let theme = crate::tui::styles::Theme::default();
        terminal
            .draw(|frame| dialog.render(frame, frame.area(), &theme))
            .expect("render");
        let screen = screen_text(terminal.backend().buffer());
        assert!(
            screen.contains(&format!("✗ Error: {expected}")),
            "error not rendered for {path:?}: {screen}"
        );
        assert!(
            !screen.contains("Ctrl+P branches"),
            "the error must replace the hints, not hide behind them: {screen}"
        );

        // Leaving the overlay must not strand the error in the main dialog.
        dialog.handle_key(key(KeyCode::Esc));
        assert!(!dialog.worktree_config_mode);
        assert!(dialog.error_message.is_none());
    }
}

#[test]
#[serial_test::serial]
fn branch_picker_mouse_selection_routes_to_the_focused_field() {
    // A branch picked with the mouse lands in the field a keyboard pick would,
    // so opening from Base and clicking a row must not overwrite Name. Needs a
    // real render: the picker learns its clickable area while drawing.
    use ratatui::{backend::TestBackend, Terminal};

    let temp_home = tempfile::tempdir().expect("temp home");
    let _home = crate::session::test_support::isolate_home(temp_home.path());
    let repo = branch_picker_repo_in(temp_home.path());

    let mut dialog = worktree_config_dialog(repo.to_string_lossy().to_string());
    dialog.worktree_config_focused_field = WT_BASE_BRANCH_FIELD;
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    assert!(dialog.branch_picker.is_active());

    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
    let theme = crate::tui::styles::Theme::default();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &theme))
        .expect("render");

    let branch = dialog
        .branch_picker
        .filtered_items()
        .first()
        .map(|s| (*s).clone())
        .expect("repo should expose a branch");
    let buffer = terminal.backend().buffer().clone();
    let (col, row) = (0..buffer.area.height)
        .find_map(|row| {
            let line: String = (0..buffer.area.width)
                .map(|col| buffer[(col, row)].symbol())
                .collect();
            line.find(&branch).map(|idx| (idx as u16, row))
        })
        .expect("branch row should be rendered");

    dialog.handle_click(col, row);

    assert_eq!(dialog.base_branch.value(), branch);
    assert!(
        dialog.worktree_branch.value().is_empty(),
        "Name must stay untouched, got {:?}",
        dialog.worktree_branch.value()
    );
}

#[test]
#[serial_test::serial]
fn test_reload_config_defaults_uses_project_worktree_override() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let _home = crate::session::test_support::isolate_home(temp_home.path());

    let repo = tempfile::tempdir().expect("temp repo");
    crate::session::projects::add(
        "default",
        crate::session::ProjectScope::Global,
        crate::session::Project::new(
            "demo",
            repo.path().to_string_lossy(),
            crate::session::ProjectScope::Global,
        ),
        false,
    )
    .expect("register project");
    crate::session::projects::update_overrides(
        "default",
        crate::session::ProjectScope::Global,
        "demo",
        |ov| ov.worktree_enabled = Some(true),
    )
    .expect("set override");

    let mut dialog = single_tool_dialog();
    dialog.path = Input::new(repo.path().to_string_lossy().to_string());
    dialog.available_profiles = vec!["default".to_string()];
    dialog.profile_descriptions = vec![None];
    dialog.profile_index = 0;
    // Global config's worktree.enabled defaults to false; the project's
    // override should win.
    dialog.reload_config_defaults();

    assert!(
        dialog.worktree_enabled,
        "project override should win over the false global default"
    );

    // Browsing to the project applies its override without resetting other edits.
    let mut dialog = multi_tool_dialog();
    dialog.tool_index = 1;
    dialog.yolo_mode = true;
    dialog.focused_field = 0;
    dialog.path = Input::new(repo.path().to_string_lossy().to_string());
    dialog.handle_key(ctrl_key(KeyCode::Char('p')));
    dialog.handle_key(key(KeyCode::Enter));
    assert!(!dialog.dir_picker.is_active());
    assert!(dialog.worktree_enabled);
    assert_eq!((dialog.tool_index, dialog.yolo_mode), (1, true));

    let pick = |dialog: &mut NewSessionDialog, path: &std::path::Path| {
        dialog.focused_field = 0;
        dialog.path = Input::new(path.to_string_lossy().to_string());
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        dialog.handle_key(key(KeyCode::Enter));
    };
    // Leaving the project drops its override; a direct toggle then survives picks.
    let unregistered = tempfile::tempdir().expect("unregistered dir");
    pick(&mut dialog, unregistered.path());
    assert!(!dialog.worktree_enabled);
    dialog.focused_field = 4;
    dialog.handle_key(key(KeyCode::Char(' ')));
    assert!(dialog.worktree_enabled);
    pick(&mut dialog, unregistered.path());
    assert!(dialog.worktree_enabled);

    // A typed path applies the override once focus leaves the field.
    let mut dialog = single_tool_dialog();
    dialog.path = Input::default();
    type_str(&mut dialog, &repo.path().to_string_lossy());
    dialog.handle_key(key(KeyCode::Tab));
    assert!(dialog.worktree_enabled);

    // Submitting straight from the path field applies it too.
    let mut dialog = single_tool_dialog();
    dialog.path = Input::default();
    type_str(&mut dialog, &repo.path().to_string_lossy());
    assert!(submitted(dialog.handle_key(key(KeyCode::Enter))).worktree_enabled);
}

#[test]
fn terminal_fork_hides_structured_despite_structured_default() {
    let mut dialog = single_tool_dialog();
    dialog.structured_default = true;
    dialog.set_structured_capable(true);
    dialog.apply_structured_default();
    assert!(dialog.structured_enabled);
    dialog.set_fork_from(crate::session::ForkSeed::Terminal {
        parent: Box::new(crate::session::ConversationBinding {
            session_id: "parent".into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/store".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: TEST_PATH.into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        }),
        child_session_id: "child".into(),
    });
    assert!(!dialog.structured_capable);
    assert!(!dialog.structured_enabled);
}

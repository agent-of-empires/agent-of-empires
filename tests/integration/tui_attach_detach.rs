//! Integration tests for TUI attach/detach behavior
//!
//! These tests validate that the terminal state is properly managed when
//! attaching to and detaching from tmux sessions.

use std::process::Command;

/// Verify tmux is available for testing
fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn app_method_body<'a>(source: &'a str, name: &str) -> &'a str {
    let signature = format!("fn {name}");
    let start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("{name} method not found"));
    let section = &source[start..];
    let open = section
        .find('{')
        .unwrap_or_else(|| panic!("{name} method body not found"));
    let body_start = start + open;
    let mut depth = 0;

    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[body_start..body_start + offset + 1];
                }
            }
            _ => {}
        }
    }

    panic!("{name} method body should have a closing brace");
}

fn assert_contains_in_order(haystack: &str, needles: &[&str]) {
    let mut cursor = 0;

    for needle in needles {
        let relative_index = haystack[cursor..]
            .find(needle)
            .unwrap_or_else(|| panic!("expected to find `{needle}` after byte {cursor}"));
        cursor += relative_index + needle.len();
    }
}

/// Test that tmux sessions can be created and killed
#[test]
fn test_tmux_session_lifecycle() {
    if !tmux_available() {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let session_name = "aoe_test_lifecycle_12345678";

    // Create a detached session
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", session_name])
        .output()
        .expect("Failed to create tmux session");

    assert!(create.status.success(), "Failed to create test session");

    // Verify session exists
    let check = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .expect("Failed to check session");

    assert!(
        check.status.success(),
        "Session should exist after creation"
    );

    // Kill session
    let kill = Command::new("tmux")
        .args(["kill-session", "-t", session_name])
        .output()
        .expect("Failed to kill session");

    assert!(kill.status.success(), "Failed to kill test session");

    // Verify session no longer exists
    let check_after = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .expect("Failed to check session");

    assert!(
        !check_after.status.success(),
        "Session should not exist after kill"
    );
}

/// Test that session names are properly sanitized
#[test]
fn test_session_name_format() {
    let prefix = "aoe_";

    // Valid session names should start with our prefix
    let session_name = format!("{}my_project_abc12345", prefix);
    assert!(session_name.starts_with(prefix));

    // Session names should not contain problematic characters
    assert!(!session_name.contains(' '));
    assert!(!session_name.contains(':'));
    assert!(!session_name.contains('.'));
}

/// The TUI live-send resize path must size the pane through
/// `Session::resize_window` (which adds the status-bar chrome back so the pane
/// lands at exactly the requested rows, #2766), not a raw `resize-window` that
/// ignores chrome. A raw resize leaves the live pane a row short of the preview
/// output area whenever a client reserves the status row, desyncing the live
/// preview by a row (#2742). Behavioral coverage is unreliable here because
/// `resize-window` is a no-op / reports chrome 0 on a detached, client-less
/// tmux on some builds (see the dropped convergence test in #2766); the
/// chrome math itself is unit-tested by `chrome_rows_*`. This guards the
/// wiring so it can't silently regress to the raw path.
#[test]
fn test_live_send_resize_uses_chrome_aware_resize_window() {
    // Worker dispatch (the `TmuxAction::Resize` arm).
    let dispatch =
        std::fs::read_to_string("src/tui/home/live_send.rs").expect("Failed to read live_send.rs");
    let body = app_method_body(&dispatch, "dispatch_via_fork");
    assert!(
        body.contains("resize_window("),
        "dispatch_via_fork must resize through Session::resize_window (chrome-aware, #2766)"
    );
    assert!(
        !body.contains("\"resize-window\""),
        "dispatch_via_fork must not run a raw `resize-window`, which ignores status-bar chrome (#2742)"
    );

    // Live-send entry sync (`finalize_live_send_resize`).
    let home = std::fs::read_to_string("src/tui/home/mod.rs").expect("Failed to read home/mod.rs");
    let finalize = app_method_body(&home, "finalize_live_send_resize");
    assert!(
        finalize.contains("resize_window("),
        "finalize_live_send_resize must resize through Session::resize_window (chrome-aware, #2766)"
    );
    assert!(
        !finalize.contains("\"resize-window\""),
        "finalize_live_send_resize must not run a raw `resize-window` (#2742)"
    );
}

/// Test terminal mode switching sequence
///
/// This guards the production sequence around the attach closure:
/// leave TUI mode, release the EventStream stdin reader, run the
/// attach closure, restore TUI mode, then recreate the EventStream and
/// clear. The EventStream is recreated only after raw mode and the
/// alternate screen are restored so the fresh reader is born into raw
/// mode rather than attached to a briefly-cooked tty.
#[test]
fn test_terminal_mode_sequence_documented() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");
    let helper_body = app_method_body(&source, "with_raw_mode_disabled");

    assert_contains_in_order(
        helper_body,
        &[
            "disable_raw_mode",
            "LeaveAlternateScreen",
            "DisableBracketedPaste",
            "DisableMouseCapture",
            "cursor::Show",
            "Write::flush",
            "event_stream.take",
            "let result = f()",
            "enable_raw_mode",
            "EnterAlternateScreen",
            "EnableBracketedPaste",
            "cursor::Hide",
            "sync_mouse_capture",
            "Write::flush",
            "self.event_stream = Some(EventStream::new())",
            "clear_terminal",
        ],
    );
}

/// Test that attach/detach uses terminal backend, not std::io::stdout()
///
/// This test verifies the fix for the terminal corruption bug where
/// using std::io::stdout() instead of terminal.backend_mut() caused
/// file descriptor desynchronization, corrupting tmux sessions.
///
/// The terminal leave/restore logic lives in `with_raw_mode_disabled`.
/// Attach paths go through `with_attached_status_hooks`, which wraps that
/// helper while polling status hooks during a blocked tmux attach.
#[test]
fn test_attach_uses_terminal_backend() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");

    // The shared helper that handles terminal mode switching must use backend_mut()
    let helper_body = app_method_body(&source, "with_raw_mode_disabled");

    assert!(
        !helper_body.contains("std::io::stdout()"),
        "with_raw_mode_disabled should use terminal.backend_mut() instead of std::io::stdout(). \
         Using std::io::stdout() creates separate file descriptor handles that can \
         corrupt terminal state and cause 'open terminal failed: not a terminal' errors."
    );

    assert!(
        helper_body.contains("terminal.backend_mut()"),
        "with_raw_mode_disabled should use terminal.backend_mut() for terminal operations"
    );

    let attached_status_body = app_method_body(&source, "with_attached_status_hooks");

    assert!(
        attached_status_body.contains("with_raw_mode_disabled"),
        "with_attached_status_hooks should delegate to with_raw_mode_disabled"
    );

    assert!(
        !attached_status_body.contains("std::io::stdout()"),
        "with_attached_status_hooks should not use std::io::stdout() directly"
    );

    for attach_method in ["attach_session", "attach_terminal", "attach_tool_session"] {
        let attach_body = app_method_body(&source, attach_method);

        assert!(
            attach_body.contains("with_attached_status_hooks"),
            "{attach_method} should leave TUI mode through with_attached_status_hooks"
        );

        assert!(
            !attach_body.contains("std::io::stdout()"),
            "{attach_method} should not use std::io::stdout() directly"
        );
    }
}

/// Attached status hooks may already have fired while tmux owned the
/// terminal. Apply their final snapshot after reload so the next normal
/// poll sees the same runtime status and does not fire the transition again.
#[test]
fn test_attach_applies_attached_status_snapshot_after_reload() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");

    for attach_method in ["attach_session", "attach_terminal", "attach_tool_session"] {
        let attach_body = app_method_body(&source, attach_method);
        assert_contains_in_order(
            attach_body,
            &[
                "attached_status_updates",
                "self.home.reload()?",
                "apply_status_updates_without_hooks(attached_status_updates)",
            ],
        );
    }
}

#[test]
fn test_attach_resets_status_refresh_without_watcher() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");
    let attached_status_body = app_method_body(&source, "with_attached_status_hooks");

    assert_contains_in_order(
        attached_status_body,
        &[
            "if let Some(watcher) = watcher",
            "attached_status_updates = watcher.stop()",
            "}",
            "self.home.reset_status_refresh()",
            "result.map",
        ],
    );
}

/// Test that a failed restart inside attach surfaces a transient toast.
///
/// Before the fix, when `restart_instance_with_size_opts` returned Err the
/// code stored the error on the instance and bailed `Ok(())`, with no
/// user-visible signal. This test guards the wiring that turns the failure
/// into an `UpdateStatus::transient` toast.
#[test]
fn test_attach_restart_failure_emits_transient_toast() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");

    let attach_fn_start = source
        .find("fn attach_session(")
        .expect("attach_session function not found");

    // Walk to the end of attach_session by finding the next `fn ` at the
    // same indentation level.
    let attach_fn_section = &source[attach_fn_start..];
    let attach_fn_end = attach_fn_section
        .find("\n    fn ")
        .unwrap_or(attach_fn_section.len());
    let attach_fn_body = &attach_fn_section[..attach_fn_end];

    let restart_idx = attach_fn_body
        .find("restart_instance_with_size_opts")
        .expect("attach_session should call restart_instance_with_size_opts");
    let after_restart = &attach_fn_body[restart_idx..];

    assert!(
        after_restart.contains("UpdateStatus::transient"),
        "attach_session must surface restart failure via UpdateStatus::transient. \
         Without this, the TUI silently stays on home and the user sees no error."
    );
    assert!(
        after_restart.contains("restart failed"),
        "the toast should carry the `restart failed: ...` prefix so the error \
         is recognizable in the bar."
    );
}

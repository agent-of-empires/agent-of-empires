//! Source-level guards for the TUI attach/detach terminal handoff.

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

/// The TUI live-send resize path must queue geometry through `LiveSendWorker`.
/// The worker dispatches through `Session::resize_window_if_owner`, preserving
/// chrome-aware sizing while fencing ownership in tmux's command queue (#2766).
/// A synchronous prep resize races the worker's ownership claim; a raw
/// `resize-window` also ignores chrome and leaves the pane a row short (#2742).
/// The chrome math and worker behavior have direct tests; this guards the
/// cross-module wiring.
#[test]
#[serial_test::parallel]
fn test_live_send_resize_uses_chrome_aware_resize_window() {
    let dispatch =
        std::fs::read_to_string("src/tui/home/live_send.rs").expect("Failed to read live_send.rs");
    let body = app_method_body(&dispatch, "dispatch_via_fork");
    assert!(
        body.contains("resize_window_if_owner("),
        "dispatch_via_fork must use the guarded chrome-aware resize path (#2766)"
    );
    assert!(
        !body.contains("\"resize-window\""),
        "dispatch_via_fork must not run a raw resize-window (#2742)"
    );

    let render =
        std::fs::read_to_string("src/tui/home/render.rs").expect("Failed to read render.rs");
    let reconcile = app_method_body(&render, "resize_live_pane_if_target");
    assert!(
        reconcile.contains("worker.resize(width, height)"),
        "render must queue settled geometry through LiveSendWorker"
    );
    assert!(
        !reconcile.contains("resize_window("),
        "render must not synchronously resize on the paint path"
    );

    let prep = std::fs::read_to_string("src/tui/home/live_send_prep.rs")
        .expect("Failed to read home/live_send_prep.rs");
    assert!(
        !prep.contains("resize_window("),
        "live-send preparation must leave post-draw resize ownership to the worker"
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
#[serial_test::parallel]
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
#[serial_test::parallel]
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

    for attach_method in [
        "attach_live_session",
        "attach_terminal",
        "attach_tool_session",
    ] {
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
#[serial_test::parallel]
fn test_attach_applies_attached_status_snapshot_after_reload() {
    let source = std::fs::read_to_string("src/tui/app.rs").expect("Failed to read app.rs");

    // Every attach path hands its hook snapshot to one settle helper, which
    // owns the reload-then-apply order.
    for attach_method in [
        "attach_live_session",
        "attach_terminal",
        "attach_tool_session",
    ] {
        let attach_body = app_method_body(&source, attach_method);
        assert_contains_in_order(
            attach_body,
            &[
                "attached_status_updates",
                "self.settle_after_attach(attached_status_updates)?",
            ],
        );
    }
    assert_contains_in_order(
        app_method_body(&source, "settle_after_attach"),
        &["self.home.reload()?", "apply_status_updates_without_hooks("],
    );
}

#[test]
#[serial_test::parallel]
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

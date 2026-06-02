use serial_test::serial;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[serial]
fn test_new_session_dialog_opens() {
    require_tmux!();

    let mut h = TuiTestHarness::new("new_dialog");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("n");
    h.wait_for("Title");
    h.assert_screen_contains("Path");
}

#[test]
#[serial]
fn test_new_session_dialog_escape_cancels() {
    require_tmux!();

    let mut h = TuiTestHarness::new("new_esc");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("n");
    h.wait_for("Title");

    h.send_keys("Escape");
    h.wait_for_absent("Title", Duration::from_secs(5));
    // Back to home screen.
    h.assert_screen_contains("No sessions yet");
}

#[test]
#[serial]
fn test_left_click_on_empty_sidebar_is_inert_outside_live_mode() {
    // The empty-sidebar left-click intentionally does NOT open the
    // new-session dialog; that entry point moved to the right-click
    // empty-sidebar menu. Verifies the click is absorbed without
    // popping a modal so a stray click while reading the sidebar
    // doesn't summon an unexpected dialog.
    require_tmux!();

    let mut h = TuiTestHarness::new("empty_lclick");
    h.spawn_tui();

    h.wait_for(" aoe ");
    // Dismiss the first-run welcome dialog so the sidebar is the
    // top-most surface receiving clicks.
    h.send_keys("Enter");
    h.wait_for("No sessions yet");

    // Click well below the empty-state label in the sidebar column.
    h.send_mouse_click(0, 10, 15);

    // No dialog should have opened.
    std::thread::sleep(Duration::from_millis(300));
    let screen = h.capture_screen();
    assert!(
        !screen.contains(" New Session "),
        "left-click on empty sidebar must not open new-session anymore\nscreen:\n{screen}"
    );
    assert!(
        screen.contains("No sessions yet"),
        "home view should still be showing the empty-state copy\nscreen:\n{screen}"
    );
}

#[test]
#[serial]
fn test_ctrl_p_browse_dir_picker_renders_as_full_overlay() {
    // Regression: the dir picker's render call used to receive a
    // local `area` shadowed by the per-field layout chunks, so the
    // picker ended up clamped inside the Group row's 1-line strip
    // and was unusable. Verify the picker renders at a meaningful
    // size (more than a single line and wide enough for its filter
    // input + at least one directory entry).
    require_tmux!();

    let mut h = TuiTestHarness::new("ctrl_p_picker");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("No sessions yet");
    h.send_keys("n");
    h.wait_for(" New Session ");
    // Path is the default focused field; Ctrl+P opens the dir picker.
    h.send_keys("C-p");
    h.wait_for("Browse:");
    let screen = h.capture_screen();
    assert!(
        screen.contains("Filter:"),
        "dir picker should render its Filter input\nscreen:\n{screen}"
    );
    assert!(
        screen.contains("../"),
        "dir picker should list at least the parent-dir entry\nscreen:\n{screen}"
    );
    // The picker has its own hint line; if it rendered crammed into
    // the underlying form's hint chunk this would be missing.
    assert!(
        screen.contains("Enter open/select"),
        "dir picker should render its full hint line\nscreen:\n{screen}"
    );
}

#[test]
#[serial]
fn test_right_click_on_empty_sidebar_opens_context_menu() {
    // The right-click menu on the empty area lists the three actions
    // that used to be keyboard-only entry points: New Session, Change
    // Sort, Change Grouping. Verifies the menu opens, lists the three
    // items, and Escape dismisses without dispatching.
    require_tmux!();

    let mut h = TuiTestHarness::new("empty_rclick");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("No sessions yet");

    // SGR button code 2 = right click.
    h.send_mouse_click(2, 10, 15);

    h.wait_for("New Session");
    h.assert_screen_contains("Change Sort");
    h.assert_screen_contains("Change Grouping");

    h.send_keys("Escape");
    h.wait_for_absent("Change Sort", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

#[test]
#[serial]
fn test_right_click_on_session_row_opens_rename_delete_menu() {
    // Right-click on an existing session row opens the per-row
    // Rename / Delete menu (a different menu variant than the empty
    // area version). Verifies the menu copy and that Escape dismisses
    // without opening either follow-up dialog. The session is created
    // out-of-band via `aoe add` so the test doesn't have to drive the
    // new-session dialog (which has its own coverage elsewhere).
    require_tmux!();

    let mut h = TuiTestHarness::new("session_rclick");
    // Seed a session before launching the TUI.
    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "RClickRow"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("RClickRow");

    // Right-click on the first session row. The row sits inside the
    // bordered sidebar panel: top border is row 1, the first item is
    // row 2. Column 5 lands on the row's label area.
    h.send_mouse_click(2, 5, 2);

    h.wait_for("Rename");
    h.assert_screen_contains("Delete");
    // The empty-sidebar menu items must NOT appear here; verifies
    // that the row-aware menu opened, not the empty-area variant.
    let screen = h.capture_screen();
    assert!(
        !screen.contains("Change Sort"),
        "session menu should not show Change Sort\nscreen:\n{screen}"
    );

    h.send_keys("Escape");
    h.wait_for_absent("Rename", Duration::from_secs(5));
}

/// Submit the new session dialog, handling the "Path does not exist. Create?"
/// prompt if it appears.
///
/// macOS CI tmux occasionally drops the first Enter when sent right after a
/// long literal-text burst, leaving the dialog stuck in the input state. We
/// detect that by polling for the dialog to transition (close, switch to a
/// loading overlay, or show the create-dir prompt) and re-send Enter if the
/// dialog still shows its hint line after a grace period. Resending Enter is
/// idempotent: by the time the dialog has closed it is already gone, so a
/// late-arriving second Enter falls through to the home view, where Enter is
/// a no-op when no session row is selected (the Creating stub is auto-selected
/// only after the dialog closes).
fn submit_new_session_dialog(h: &TuiTestHarness) {
    h.send_keys("Enter");
    let start = std::time::Instant::now();
    let mut resent = false;
    loop {
        let screen = h.capture_screen();
        if screen.contains("Path does not exist") {
            h.send_keys("y");
            return;
        }
        // Any of these means Enter was accepted by the dialog.
        if !screen.contains(" New Session ")
            || screen.contains("Running Hooks")
            || screen.contains("Creating Session")
            || screen.contains("Creating...")
        {
            return;
        }
        if !resent && start.elapsed() > Duration::from_millis(800) {
            // Dialog still in input state. Assume Enter was lost; resend.
            h.send_keys("Enter");
            resent = true;
        }
        if start.elapsed() > Duration::from_secs(5) {
            // Give up; downstream wait_for will produce the diagnostic.
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Write a global config with on_create hooks so session creation goes through
/// the background CreationPoller and shows a Creating stub in the session list.
fn write_config_with_hooks(h: &TuiTestHarness, hook_cmd: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[hooks]
on_create = ["{hook_cmd}"]

[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true
"#,
        hook_cmd = hook_cmd,
        version = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(config_dir.join("config.toml"), config_content)
        .expect("write config with hooks");
}

#[test]
#[serial]
fn test_creating_stub_appears_during_hook_execution() {
    require_tmux!();

    let mut h = TuiTestHarness::new("creating_stub");
    // Use a slow hook so we can observe the Creating state.
    write_config_with_hooks(&h, "sleep 5");
    let project = h.project_path();
    h.spawn_tui();

    h.wait_for(" aoe ");

    // Open new session dialog and fill in the path.
    h.send_keys("n");
    h.wait_for("Title");
    // Tab from Title to Path field.
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(&h);

    // The dialog should close and a Creating stub should appear in the list.
    // The preview pane shows "Creating..." with hook output.
    h.wait_for_timeout("Creating...", Duration::from_secs(10));
    h.assert_screen_contains("Hook Output");
}

#[test]
#[serial]
fn test_creating_stub_cancelled_with_ctrl_c() {
    require_tmux!();

    let mut h = TuiTestHarness::new("creating_cancel");
    write_config_with_hooks(&h, "sleep 10");
    let project = h.project_path();
    h.spawn_tui();

    h.wait_for(" aoe ");

    // Create a session with a slow hook.
    h.send_keys("n");
    h.wait_for("Title");
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(&h);

    h.wait_for_timeout("Creating...", Duration::from_secs(10));

    // Cancel with Ctrl+C.
    h.send_keys("C-c");

    // The Creating stub should be removed and we should be back to empty state.
    h.wait_for_absent("Creating...", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

#[test]
#[serial]
fn test_creating_blocks_second_session_creation() {
    require_tmux!();

    let mut h = TuiTestHarness::new("creating_blocks_new");
    write_config_with_hooks(&h, "sleep 10");
    let project = h.project_path();
    h.spawn_tui();

    h.wait_for(" aoe ");

    // Start creating a session.
    h.send_keys("n");
    h.wait_for("Title");
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(&h);

    h.wait_for_timeout("Creating...", Duration::from_secs(10));

    // Try to create another session while one is in progress.
    h.send_keys("n");

    // Should show an info dialog instead of the new session dialog.
    h.wait_for_timeout("Please Wait", Duration::from_secs(3));
    h.assert_screen_contains("already being created");

    // Clean up.
    h.send_keys("Enter");
    h.send_keys("C-c");
    h.wait_for_absent("Creating...", Duration::from_secs(5));
}

#[test]
#[serial]
fn test_quit_during_creation_shows_confirm() {
    require_tmux!();

    let mut h = TuiTestHarness::new("quit_creating");
    write_config_with_hooks(&h, "sleep 10");
    let project = h.project_path();
    h.spawn_tui();

    h.wait_for(" aoe ");

    // Start creating a session.
    h.send_keys("n");
    h.wait_for("Title");
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(&h);

    h.wait_for_timeout("Creating...", Duration::from_secs(10));

    // Navigate away from the creating stub so Ctrl+C triggers quit path.
    // With only one session (the stub), pressing 'q' is more reliable.
    h.send_keys("q");

    // Should show a confirmation dialog instead of quitting.
    // Use a 5s timeout (the convention in this file) so a CI runner under
    // load has enough headroom for tmux capture-pane to see the dialog
    // after the `q` key triggers the state transition; the previous 3s
    // budget was a flake source on ubuntu-latest (empty screen captures
    // mid-render).
    h.wait_for_timeout("Session Creating", Duration::from_secs(5));
    h.assert_screen_contains("Quit anyway");

    // Decline to quit.
    h.send_keys("n");
    h.wait_for_absent("Session Creating", Duration::from_secs(5));
    // TUI is still running with the Creating stub.
    h.assert_screen_contains("Creating...");

    // Clean up by cancelling creation (stub is selected again).
    h.send_keys("C-c");
    h.wait_for_absent("Creating...", Duration::from_secs(5));
}

/// Write a global config that opts into `new_session_attach_mode = "live_send"`
/// so creation routes into live-send mode instead of the historical tmux
/// attach. No hooks; the sync create path applies (this is the path that
/// originally bypassed the setting).
fn write_config_attach_mode_live_send(h: &TuiTestHarness) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true

[session]
new_session_attach_mode = "live_send"
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(config_dir.join("config.toml"), config_content)
        .expect("write config with attach mode");
}

/// Regression guard for the original "new sessions still attach to tmux even
/// though I picked live mode" bug. Both creation paths (sync and async) must
/// route through `dispatch_new_session_attach` and honor the setting; the
/// sync path was the one that bypassed it (the symptom that made this PR
/// happen in the first place).
#[test]
#[serial]
fn test_new_session_enters_live_mode_when_configured() {
    require_tmux!();

    let mut h = TuiTestHarness::new("attach_live_send");
    write_config_attach_mode_live_send(&h);
    let project = h.project_path();
    h.spawn_tui();

    h.wait_for(" aoe ");

    h.send_keys("n");
    h.wait_for("Title");
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(&h);

    // After creation, the home view stays mounted with the LIVE banner in
    // the footer. A tmux-attach dispatch would replace the entire TUI
    // screen with whatever the agent is rendering, so the banner is the
    // load-bearing tell that the setting was respected.
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    // Sanity: the home view's title chrome is still on screen, meaning
    // the dispatch didn't flip into the tmux attach view.
    h.assert_screen_contains(" aoe ");
}

// NOTE: a previous version of this file added
// `test_live_send_repeated_entry_exit_remains_responsive`, which
// drove the TUI through two Tab → C-q cycles to validate the
// `ControlModeClient` spawn/drop lifecycle. The test was reliable on
// macOS but flaked on ubuntu-latest because the pane process (a
// short-lived shell, picked by the wizard when no agent is
// installed) exited cleanly within ~2s of session creation: by the
// time the second `Tab` fired, `ensure_pane_ready` saw a dead pane
// and surfaced the "Live send failed" dialog instead of LIVE. The
// e2e test conflated two concerns ("the client lifecycle is clean"
// vs. "the pane survives across cycles"), so the lifecycle
// assertion now lives in `tests/integration/tmux_control_mode.rs`
// (`control_mode_spawn_drop_respawn_against_same_session`), which
// spawns against a raw tmux session that doesn't go anywhere.

/// Init a directory as a git repo so `aoe project add` accepts it.
fn init_git_repo_for_project(path: &std::path::Path) {
    use std::process::Command;
    std::fs::create_dir_all(path).expect("create repo dir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {:?} failed", args);
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "-q", "-m", "init"]);
}

#[test]
#[serial]
fn test_new_session_from_saved_project_prefills_path() {
    require_tmux!();

    let mut h = TuiTestHarness::new("new_from_project");
    // Seed several projects so the picker's filter has something to narrow.
    for name in ["frontend", "backend", "mobile"] {
        let repo = h.home_path().join(name);
        init_git_repo_for_project(&repo);
        let add = h.run_cli(&["project", "add", repo.to_str().unwrap()]);
        assert!(
            add.status.success(),
            "project add {name} failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss first-run welcome
    h.wait_for("No sessions yet");

    // `b` opens the saved-project picker, which lists every registered repo.
    h.send_keys("b");
    h.wait_for("New Session from Project");
    h.assert_screen_contains("frontend");
    h.assert_screen_contains("backend");
    h.assert_screen_contains("mobile");

    // Typing filters the list; "mob" narrows to the single "mobile" project.
    // Send one char at a time: `type_text`'s literal (`-l`) mode arrives as a
    // bracketed paste, which the picker's filter input doesn't capture.
    h.send_keys("m");
    h.send_keys("o");
    h.send_keys("b");
    h.wait_for_absent("frontend", std::time::Duration::from_secs(5));
    h.assert_screen_not_contains("backend");
    h.assert_screen_contains("mobile");

    // Selecting the filtered match opens the new-session dialog pre-filled
    // with that project's path.
    h.send_keys("Enter");
    h.wait_for("Title");
    h.assert_screen_contains("Path");
    h.assert_screen_contains("mobile");
}

#[test]
#[serial]
fn test_new_session_from_project_empty_state() {
    require_tmux!();

    let mut h = TuiTestHarness::new("new_from_project_empty");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss first-run welcome
    h.wait_for("No sessions yet");

    // With no saved projects, `b` surfaces the info dialog instead.
    h.send_keys("b");
    h.wait_for("No Projects");
}

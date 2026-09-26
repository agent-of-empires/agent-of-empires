use serial_test::parallel;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[parallel]
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
#[parallel]
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
#[parallel]
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
#[parallel]
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
#[parallel]
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

/// Write a global config that keeps existing-session activation at its
/// historical tmux default while opening newly-created sessions in live mode.
/// No hooks; the sync create path applies.
fn write_config_new_session_mode_live_send(h: &TuiTestHarness) {
    let config_path = app_dir_in(h.home_path()).join("config.toml");
    let config_content = std::fs::read_to_string(&config_path).expect("read harness config");
    std::fs::write(
        app_dir_in(h.home_path()).join("state.toml"),
        format!(
            "has_seen_welcome = true\nhas_responded_to_telemetry = true\nlast_seen_version = \"{}\"\nhas_acknowledged_agent_hooks = true\n",
            env!("CARGO_PKG_VERSION")
        ),
    )
    .expect("write state with harness flags and hook acknowledgment");
    std::fs::write(
        config_path,
        format!("{config_content}\n[session]\nnew_session_mode = \"live_send\"\n"),
    )
    .expect("write config with new session mode");
}

/// New-session mode must remain independent from the setting that controls
/// Enter and double-click for existing sessions.
#[test]
#[parallel]
fn test_new_session_enters_live_mode_when_configured() {
    require_tmux!();

    let mut h = TuiTestHarness::new("attach_live_send");
    write_config_new_session_mode_live_send(&h);
    // The daemon starts the agent while creating, so the pane must outlive the
    // attach this test is about: a stub agent that exits immediately would make
    // the live-send preparation fail for a reason unrelated to the mode.
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 120\n").unwrap();
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

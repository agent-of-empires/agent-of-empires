//! The new-session dialog, its entry points, and the creating stub.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

/// Submit the new session dialog, answering the "Path does not exist. Create?"
/// prompt if it appears.
///
/// macOS CI tmux occasionally drops the first Enter sent right after a long
/// literal-text burst, so this resends once if the dialog is still in its input
/// state. A late second Enter is harmless: the home view ignores it while the
/// Creating stub is not yet selected.
fn submit_new_session_dialog(h: &TuiTestHarness) {
    h.send_keys("Enter");
    let start = Instant::now();
    let mut resent = false;
    loop {
        let screen = h.capture_screen();
        if screen.contains("Path does not exist") {
            h.send_keys("y");
            return;
        }
        // Any of these means the dialog accepted the Enter.
        if !screen.contains(" New Session ")
            || screen.contains("Running Hooks")
            || screen.contains("Creating Session")
            || screen.contains("Creating...")
        {
            return;
        }
        if !resent && start.elapsed() > Duration::from_millis(800) {
            h.send_keys("Enter");
            resent = true;
        }
        if start.elapsed() > Duration::from_secs(5) {
            // Give up; the downstream wait_for produces the diagnostic.
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Fill the dialog's Path field with the harness project and submit.
fn create_session_from_dialog(h: &TuiTestHarness, project: &std::path::Path) {
    h.send_keys("n");
    h.wait_for("Title");
    h.send_keys("Tab");
    h.type_text(project.to_str().unwrap());
    submit_new_session_dialog(h);
}

/// The dir picker renders as a full overlay. It used to be clamped into the
/// Group row's 1-line strip by a shadowed `area`, which made it unusable.
#[test]
#[parallel]
fn test_ctrl_p_browse_dir_picker_renders_as_full_overlay() {
    require_tmux!();
    let mut h = TuiTestHarness::new("ctrl_p_picker");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("No sessions yet");
    h.send_keys("n");
    h.wait_for(" New Session ");

    // Path is the default focused field.
    h.send_keys("C-p");
    h.wait_for("Browse:");
    let screen = h.capture_screen();
    for expected in ["Filter:", "../", "Enter open/select"] {
        assert!(
            screen.contains(expected),
            "dir picker should render {expected:?}\nscreen:\n{screen}"
        );
    }
}

/// A session whose on_create hooks are still running shows a Creating stub with
/// its hook output, blocks a second creation, warns before quitting, and is
/// removed by Ctrl+C.
#[test]
#[parallel]
fn test_creating_stub_lifecycle() {
    require_tmux!();
    let mut h = TuiTestHarness::new("creating_stub");
    // A slow hook holds the session in the Creating state.
    h.append_config("[hooks]\non_create = [\"sleep 10\"]");
    let project = h.project_path();
    h.spawn_tui();
    h.wait_for(" aoe ");

    create_session_from_dialog(&h, &project);
    h.wait_for_timeout("Creating...", Duration::from_secs(10));
    h.assert_screen_contains("Hook Output");

    h.send_keys("n");
    h.wait_for_timeout("Please Wait", Duration::from_secs(3));
    h.assert_screen_contains("already being created");
    h.send_keys("Enter");

    h.send_keys("q");
    h.wait_for_timeout("Session Creating", Duration::from_secs(5));
    h.assert_screen_contains("Quit anyway");
    h.send_keys("n");
    h.wait_for_absent("Session Creating", Duration::from_secs(5));
    h.assert_screen_contains("Creating...");

    h.send_keys("C-c");
    h.wait_for_absent("Creating...", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

/// `session.new_session_mode` is independent of the setting that controls Enter
/// and double-click for existing sessions.
#[test]
#[parallel]
fn test_new_session_enters_live_mode_when_configured() {
    require_tmux!();
    let mut h = TuiTestHarness::new("attach_live_send");
    h.append_config("[session]\nnew_session_mode = \"live_send\"");
    let project = h.project_path();
    h.spawn_tui();
    h.wait_for(" aoe ");

    create_session_from_dialog(&h, &project);

    // A tmux-attach dispatch would replace the whole screen, so the footer
    // banner plus the home chrome is the tell that live mode was used.
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    h.assert_screen_contains(" aoe ");
}

/// `N` opens the form from the row under the cursor: a group row gives its group, a session
/// row its group and its agent too.
#[test]
#[parallel]
fn test_new_from_selection_starts_on_the_selected_sessions_agent() {
    require_tmux!();
    let mut h = TuiTestHarness::new("new_from_selection_agent");
    h.install_path_command("codex");
    let project = h.project_path();
    h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        "codex-source",
        "--tool",
        "codex",
        "-g",
        "work",
    ]);
    h.spawn_tui();
    h.wait_for("codex-source");

    let new_from_selection_shows = |row: &str, tool: &str| {
        h.send_keys("N");
        h.wait_for(" New Session ");
        let screen = h.capture_screen();
        let tool_row = screen.lines().find_map(|line| {
            let rest = &line[line.find("Tool: [")? + "Tool: [".len()..];
            let (digit, rest) = rest.split_once("] ")?;
            digit.parse::<u8>().ok()?;
            Some(rest.split_whitespace().next()?.to_string())
        });
        assert_eq!(
            tool_row.as_deref(),
            Some(tool),
            "N on the {row} row should show {tool} on the numbered tool row\nscreen:\n{screen}"
        );
        assert!(
            screen.contains("Group: work"),
            "N on the {row} row should show the work group\nscreen:\n{screen}"
        );
        h.send_keys("Escape");
        h.wait_for_absent(" New Session ", Duration::from_secs(5));
    };

    new_from_selection_shows("group", "claude");
    h.send_keys("j");
    new_from_selection_shows("session", "codex");
}

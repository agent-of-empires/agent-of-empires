//! The new-session dialog, its entry points, and the creating stub.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{init_git_repo, require_tmux, TuiTestHarness};

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

#[test]
#[parallel]
fn test_new_session_dialog_opens_and_escape_cancels() {
    require_tmux!();
    let mut h = TuiTestHarness::new("new_dialog");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("n");
    h.wait_for("Title");
    h.assert_screen_contains("Path");

    h.send_keys("Escape");
    h.wait_for_absent("Title", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

/// The empty-sidebar left click is absorbed: the new-session entry point moved
/// to the right-click menu, which also carries the two keyboard-only actions.
#[test]
#[parallel]
fn test_empty_sidebar_click_menu() {
    require_tmux!();
    let mut h = TuiTestHarness::new("empty_click");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome so the sidebar takes the clicks
    h.wait_for("No sessions yet");

    // Well below the empty-state label in the sidebar column.
    h.send_mouse_click(0, 10, 15);
    std::thread::sleep(Duration::from_millis(300));
    h.assert_screen_not_contains(" New Session ");
    h.assert_screen_contains("No sessions yet");

    h.send_mouse_click(2, 10, 15);
    h.wait_for("New Session");
    h.assert_screen_contains("Change Sort");
    h.assert_screen_contains("Change Grouping");

    h.send_keys("Escape");
    h.wait_for_absent("Change Sort", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

/// Right-clicking a session row opens the row menu, not the empty-area one.
#[test]
#[parallel]
fn test_right_click_on_session_row_opens_rename_delete_menu() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_rclick");
    let project = h.project_path();
    h.run_cli_ok(&["add", project.to_str().unwrap(), "-t", "RClickRow"]);

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("RClickRow");

    // Row 1 is the sidebar's top border, so the first item is row 2.
    h.send_mouse_click(2, 5, 2);
    h.wait_for("Rename");
    h.assert_screen_contains("Delete");
    h.assert_screen_not_contains("Change Sort");

    h.send_keys("Escape");
    h.wait_for_absent("Rename", Duration::from_secs(5));
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

/// `b` opens the saved-project picker, which filters and prefills the dialog.
#[test]
#[parallel]
fn test_new_session_from_saved_project_prefills_path() {
    require_tmux!();
    // Rooted at `/tmp/.tmpXXXXXX`, so the path segments cannot match the
    // `-app` filter below.
    let mut h = TuiTestHarness::new_in_tmp("new_from_project");
    for name in ["frontend", "backend", "mobile-app"] {
        let repo = h.home_path().join(name);
        init_git_repo(&repo);
        h.run_cli_ok(&["project", "add", repo.to_str().unwrap()]);
    }

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("No sessions yet");

    h.send_keys("b");
    h.wait_for("New Session from Project");
    h.assert_screen_contains("frontend");
    h.assert_screen_contains("backend");
    h.assert_screen_contains("mobile-app");

    // One key at a time: `type_text` arrives as a bracketed paste, which the
    // filter input does not capture.
    for key in ["-", "a", "p", "p"] {
        h.send_keys(key);
    }
    h.wait_for_absent("frontend", Duration::from_secs(5));
    h.assert_screen_not_contains("backend");
    h.assert_screen_contains("mobile-app");

    h.send_keys("Enter");
    h.wait_for("Title");
    h.assert_screen_contains("Path");
    h.assert_screen_contains("mobile");
}

/// With no saved projects, `b` opens the project add form, which Esc cancels in
/// one press.
#[test]
#[parallel]
fn test_new_session_from_project_empty_state_opens_add_form() {
    require_tmux!();
    let mut h = TuiTestHarness::new("new_from_project_empty");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("Enter"); // dismiss welcome
    h.wait_for("No sessions yet");

    h.send_keys("b");
    h.wait_for(" Projects ");
    h.assert_screen_contains("Path:");
    h.assert_screen_contains("Base branch:");

    h.send_keys("Escape");
    h.wait_for_absent(" Projects ", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

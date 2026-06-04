//! E2E coverage for the first-run intro walkthrough (issue #1564).
//!
//! The shared `TuiTestHarness` pre-seeds `app_state.has_seen_welcome = true`
//! so most tests skip onboarding. These tests undo that seed before spawning
//! so we exercise the real first-run path.

use serial_test::serial;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Rewrite the harness-seeded config so the binary starts in first-run mode
/// (no `has_seen_welcome` flag). Keeps update checks off so the only popup
/// is the intro.
fn force_first_run(h: &TuiTestHarness) {
    let cfg = app_dir_in(h.home_path()).join("config.toml");
    std::fs::write(
        &cfg,
        "[updates]\nupdate_check_mode = \"off\"\n\n[app_state]\n",
    )
    .expect("rewrite config.toml");
}

fn read_config(h: &TuiTestHarness) -> String {
    let cfg = app_dir_in(h.home_path()).join("config.toml");
    std::fs::read_to_string(&cfg).unwrap_or_default()
}

#[test]
#[serial]
fn intro_walkthrough_appears_on_first_run() {
    require_tmux!();

    let mut h = TuiTestHarness::new("intro_first_run");
    force_first_run(&h);
    h.spawn_tui();

    // Title bar uses the literal string from IntroDialog::render.
    h.wait_for("Welcome to Agent of Empires");
    h.assert_screen_contains("(1/6)");
    h.assert_screen_contains("[Skip]");
    h.assert_screen_contains("[Next");
}

#[test]
#[serial]
fn intro_advances_through_pages_with_enter() {
    require_tmux!();

    let mut h = TuiTestHarness::new("intro_pages");
    force_first_run(&h);
    h.spawn_tui();

    h.wait_for("(1/6)");
    h.send_keys("Enter");
    h.wait_for("(2/6)");
    h.assert_screen_contains("Help improve aoe with anonymous usage telemetry?");
    h.send_keys("Enter");
    h.wait_for("(3/6)");
    h.assert_screen_contains("Start your first session");
    h.send_keys("Enter");
    h.wait_for("(4/6)");
    h.assert_screen_contains("How do you want to drive your sessions?");
    h.send_keys("Enter");
    h.wait_for("(5/6)");
    h.assert_screen_contains("Pick a theme");
    h.send_keys("Enter");
    h.wait_for("(6/6)");
    h.assert_screen_contains("You're all set");
    h.send_keys("Enter");
    // Intro dismissed: list view marker should appear and the dialog title
    // should be gone.
    h.wait_for("No sessions yet");
    h.wait_for_absent("(6/6)", Duration::from_secs(3));
}

#[test]
#[serial]
fn intro_esc_skips_without_changing_theme() {
    require_tmux!();

    let mut h = TuiTestHarness::new("intro_skip");
    force_first_run(&h);
    h.spawn_tui();

    h.wait_for("(1/6)");
    h.send_keys("Escape");
    h.wait_for("No sessions yet");

    // First-run flag should still flip to true (App::new sets it before
    // opening the dialog), but no theme name was written and the attach
    // mode default (Tmux) stays in place.
    let cfg = read_config(&h);
    assert!(
        cfg.contains("has_seen_welcome = true"),
        "expected has_seen_welcome=true after skip, got:\n{cfg}"
    );
    assert!(
        !cfg.contains("name = \"empire\""),
        "skip should not write a theme; got:\n{cfg}"
    );
    assert!(
        !cfg.contains("new_session_attach_mode = \"live_send\""),
        "skip should not write an attach mode; got:\n{cfg}"
    );
}

#[test]
#[serial]
fn intro_theme_pick_persists_to_config() {
    require_tmux!();

    let mut h = TuiTestHarness::new("intro_theme_save");
    force_first_run(&h);
    h.spawn_tui();

    h.wait_for("(1/6)");
    h.send_keys("Enter"); // -> page 2 (telemetry)
    h.wait_for("(2/6)");
    h.send_keys("Enter"); // -> page 3 (first session)
    h.wait_for("(3/6)");
    h.send_keys("Enter"); // -> page 4 (attach mode, pre-selects LiveSend)
    h.wait_for("(4/6)");
    h.send_keys("Enter"); // -> page 5 (theme picker)
    h.wait_for("(5/6)");
    // BUILTIN_THEMES (src/tui/styles/mod.rs) is ordered `default, empire, ...`
    // so a single Down from the default-seeded cursor lands on `empire`.
    h.send_keys("Down");
    h.send_keys("Enter"); // -> page 6 (done)
    h.wait_for("(6/6)");
    h.send_keys("Enter"); // submit

    h.wait_for("No sessions yet");

    let cfg = read_config(&h);
    assert!(
        cfg.contains("name = \"empire\""),
        "expected name = \"empire\" in config, got:\n{cfg}"
    );
    // Walking through AttachMode without toggling should persist the
    // wizard's pre-selected LiveSend on both attach-mode fields.
    assert!(
        cfg.contains("new_session_attach_mode = \"live_send\""),
        "expected new_session_attach_mode = live_send, got:\n{cfg}"
    );
    assert!(
        cfg.contains("default_attach_mode = \"live_send\""),
        "expected default_attach_mode = live_send, got:\n{cfg}"
    );
}

#[test]
#[serial]
fn intro_lets_user_choose_tmux_attach() {
    require_tmux!();

    let mut h = TuiTestHarness::new("intro_attach_tmux");
    force_first_run(&h);
    h.spawn_tui();

    h.wait_for("(1/6)");
    h.send_keys("Enter"); // -> telemetry
    h.wait_for("(2/6)");
    h.send_keys("Enter"); // -> first session
    h.wait_for("(3/6)");
    h.send_keys("Enter"); // -> attach mode
    h.wait_for("(4/6)");
    // Pre-selected LiveSend; flip to Tmux.
    h.send_keys("Down");
    h.send_keys("Enter"); // -> theme
    h.wait_for("(5/6)");
    h.send_keys("Enter"); // -> done
    h.wait_for("(6/6)");
    h.send_keys("Enter"); // submit

    h.wait_for("No sessions yet");

    let cfg = read_config(&h);
    assert!(
        cfg.contains("new_session_attach_mode = \"tmux\""),
        "expected new_session_attach_mode = tmux, got:\n{cfg}"
    );
    assert!(
        cfg.contains("default_attach_mode = \"tmux\""),
        "expected default_attach_mode = tmux, got:\n{cfg}"
    );
}

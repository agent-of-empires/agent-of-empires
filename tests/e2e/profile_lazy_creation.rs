//! Regression coverage for the lazy-profile-creation bug: naming an unknown
//! profile via `-p`/`--profile` on a read-path command must error instead of
//! silently birthing an empty `profiles/<name>/` directory. See
//! `session::resolve_existing_profile`.

use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

#[test]
#[parallel]
fn test_list_with_unknown_profile_fails_without_creating_dir() {
    let h = TuiTestHarness::new("profile_lazy_list_unknown");

    let out = h.run_cli(&["list", "-p", "ghost-profile"]);
    assert!(
        !out.status.success(),
        "aoe list -p <unknown profile> should fail"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist") && stderr.contains("aoe profile create"),
        "expected 'does not exist' + 'aoe profile create' guidance, got: {stderr}"
    );

    let ghost_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("ghost-profile");
    assert!(
        !ghost_dir.exists(),
        "merely referencing an unknown profile must not create {}",
        ghost_dir.display()
    );
}

#[test]
#[parallel]
fn test_profile_create_then_list_succeeds() {
    let h = TuiTestHarness::new("profile_lazy_create_then_list");

    let created = h.run_cli(&["profile", "create", "freshly-made"]);
    assert!(
        created.status.success(),
        "aoe profile create should succeed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let profile_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("freshly-made");
    assert!(
        profile_dir.exists(),
        "expected {} to exist after `aoe profile create`",
        profile_dir.display()
    );

    let listed = h.run_cli(&["list", "-p", "freshly-made"]);
    assert!(
        listed.status.success(),
        "aoe list -p <existing profile> should succeed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
}

/// The bare TUI launches under `--profile` and would create the profile
/// directory on open, so it is refused up front (#148). Without a TTY the
/// refusal must still be the profile error, i.e. it fires before terminal
/// setup and migrations, never after a stray directory has been minted.
#[test]
#[parallel]
fn test_tui_with_unknown_profile_fails_without_creating_dir() {
    let h = TuiTestHarness::new("profile_lazy_tui_unknown");

    let out = h.run_cli(&["-p", "ghost-profile"]);
    assert!(
        !out.status.success(),
        "aoe -p <unknown profile> should fail"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist") && stderr.contains("aoe profile create"),
        "expected 'does not exist' + 'aoe profile create' guidance, got: {stderr}"
    );

    let ghost_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("ghost-profile");
    assert!(
        !ghost_dir.exists(),
        "launching the TUI under an unknown profile must not create {}",
        ghost_dir.display()
    );
}

/// `aoe add` files the new session under `--profile`; an unknown name is
/// refused before any other argument is looked at.
#[test]
#[parallel]
fn test_add_with_unknown_profile_fails_without_creating_dir() {
    let h = TuiTestHarness::new("profile_lazy_add_unknown");

    let out = h.run_cli(&["add", "/nonexistent/aoe-e2e-path", "-p", "ghost-profile"]);
    assert!(
        !out.status.success(),
        "aoe add -p <unknown profile> should fail"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Profile 'ghost-profile' does not exist"),
        "expected the unknown-profile error first, got: {stderr}"
    );

    let ghost_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("ghost-profile");
    assert!(
        !ghost_dir.exists(),
        "`add -p <unknown>` must not create {}",
        ghost_dir.display()
    );
}

/// Commands that never consume `--profile` must not be blocked by an unknown
/// (or stale `AGENT_OF_EMPIRES_PROFILE`) value: `list --all` enumerates every
/// profile, and the daemon lifecycle verbs only look at the PID file.
#[test]
#[parallel]
fn test_profile_agnostic_commands_ignore_unknown_profile() {
    let h = TuiTestHarness::new("profile_lazy_agnostic");
    let ghost_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("ghost-profile");

    let listed = h.run_cli(&["list", "--all", "--json", "-p", "ghost-profile"]);
    assert!(
        listed.status.success(),
        "aoe list --all must not consult -p: {}",
        String::from_utf8_lossy(&listed.stderr)
    );

    // No daemon runs in the isolated home, so --status reports exactly that;
    // the point is that it reports on the daemon, not on the profile.
    let status = h.run_cli(&["serve", "--status", "-p", "ghost-profile"]);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        !stderr.contains("does not exist") && !stderr.contains("aoe profile create"),
        "aoe serve --status must not check the profile, got: {stderr}"
    );

    assert!(
        !ghost_dir.exists(),
        "profile-agnostic commands must not create {}",
        ghost_dir.display()
    );
}

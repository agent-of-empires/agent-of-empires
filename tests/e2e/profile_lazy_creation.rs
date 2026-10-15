//! Regression coverage for the lazy-profile-creation bug: naming an unknown
//! profile via `-p`/`--profile` on a read-path command must error instead of
//! silently birthing an empty `profiles/<name>/` directory. See
//! `session::require_known_profile`.

use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

/// Read-path commands refuse an unknown profile with guidance and never
/// create its directory. The bare TUI (#148) must refuse before terminal
/// setup and migrations, and `aoe add` before looking at any other argument.
#[test]
#[parallel]
fn test_unknown_profile_is_refused_without_creating_dir() {
    let h = TuiTestHarness::new("profile_lazy_unknown");
    let ghost_dir = app_dir_in(h.home_path())
        .join("profiles")
        .join("ghost-profile");

    for (args, expected) in [
        (
            vec!["list", "-p", "ghost-profile"],
            &["does not exist", "aoe profile create"][..],
        ),
        (
            vec!["-p", "ghost-profile"],
            &["does not exist", "aoe profile create"][..],
        ),
        (
            vec!["add", "/nonexistent/aoe-e2e-path", "-p", "ghost-profile"],
            &["Profile 'ghost-profile' does not exist"][..],
        ),
    ] {
        let stderr = h.run_cli_err(&args);
        for needle in expected {
            assert!(stderr.contains(needle), "{args:?}: {stderr}");
        }
        assert!(
            !ghost_dir.exists(),
            "{args:?} must not create {}",
            ghost_dir.display()
        );
    }

    h.run_cli_ok(&["profile", "create", "freshly-made"]);
    assert!(app_dir_in(h.home_path())
        .join("profiles")
        .join("freshly-made")
        .exists());
    h.run_cli_ok(&["list", "-p", "freshly-made"]);
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

//! Regression coverage for the lazy-profile-creation bug: naming an unknown
//! profile via `-p`/`--profile` on a read-path command must error instead of
//! silently birthing an empty `profiles/<name>/` directory. See
//! `session::resolve_existing_profile`.

use serial_test::serial;

use crate::harness::{app_dir_in, TuiTestHarness};

#[test]
#[serial]
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
#[serial]
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

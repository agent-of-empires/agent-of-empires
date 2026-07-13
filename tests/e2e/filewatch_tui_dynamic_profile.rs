//! e2e: dynamic profile add/remove. Catches subscription-registration bugs
//! (forgot to call `rewire_disk_subscriptions`) and forwarder wiring bugs
//! (forwarder spawned but `disk_dirty` Arc not cloned correctly).
//!
//! Flow: spawn TUI; create a new profile from the test process; switch the
//! TUI to the new profile via the picker; perform a `Storage::update`
//! against the new profile from outside the TUI; assert the TUI reflects
//! the row within sub-tick budget. Then delete the new profile and assert
//! the TUI does not panic and the picker repopulates.

use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::file_watch::FileWatchService;
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;

use crate::harness::{require_tmux, HomeGuard, TuiTestHarness};

#[test]
#[serial]
fn dynamic_profile_add_and_remove_keeps_subscriptions_in_sync() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_dyn_profile");

    let new_profile = "scratch";
    let config_dir = crate::harness::app_dir_in(h.home_path());
    std::fs::create_dir_all(config_dir.join("profiles").join(new_profile))
        .expect("seed scratch profile dir");

    h.spawn_tui();
    h.wait_for(" aoe ");

    h.send_keys("P");
    h.wait_for("Profiles");
    h.assert_screen_contains(new_profile);
    h.send_keys("Down");
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));

    let _home = HomeGuard::new(h.home_path());

    let svc: Arc<FileWatchService> = FileWatchService::noop();
    let storage = Storage::new(new_profile, svc).expect("storage for new profile");

    let title = "filewatch-dyn-row";
    storage
        .update(|i, _g| {
            let mut inst = Instance::new(title, "/tmp/filewatch-dyn");
            inst.source_profile = new_profile.to_string();
            i.push(inst);
            Ok(())
        })
        .expect("peer write to scratch profile");

    h.wait_for_timeout(title, Duration::from_millis(1_500));

    let profile_dir = config_dir.join("profiles").join(new_profile);
    std::fs::remove_dir_all(&profile_dir).expect("remove scratch profile dir");

    assert!(
        h.session_alive(),
        "TUI should not have crashed after profile dir removal"
    );

    // The active-profile filter is not auto-reset when its dir vanishes,
    // so the list header still reads `aoe [scratch]`. Switch back to
    // all-profiles mode first: with the active profile gone from the
    // list, the picker opens with "all" highlighted at the top
    // (ProfilePickerDialog::new falls back to index 0), so Enter selects
    // it and the header drops the profile tag.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));

    // Now in all-profiles mode the header carries no profile tag, so a
    // lingering "scratch" can only be a stale picker row. Reopen the
    // picker and assert the removed profile is gone. Substring "scratch"
    // rather than "scratch  0 sessions" so a stale row with any session
    // count still trips the assertion.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.wait_for_absent("scratch", Duration::from_secs(5));
    h.send_keys("Escape");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
}

#[test]
#[serial]
fn filtered_profile_switch_rewires_disk_watch_to_new_profile() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_filtered_switch");
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let alpha_dir = config_dir.join("profiles").join("alpha");
    let beta_dir = config_dir.join("profiles").join("beta");
    std::fs::create_dir_all(&alpha_dir).expect("create alpha profile dir");
    std::fs::create_dir_all(&beta_dir).expect("create beta profile dir");
    std::fs::write(
        alpha_dir.join("sessions.json"),
        r#"[{"id":"alpha-session","title":"Alpha Session","project_path":"/tmp/alpha","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2026-01-01T00:00:00Z"}]"#,
    )
    .expect("write alpha sessions.json");
    std::fs::write(
        beta_dir.join("sessions.json"),
        r#"[{"id":"beta-session","title":"Beta Session","project_path":"/tmp/beta","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2026-01-01T00:00:00Z"}]"#,
    )
    .expect("write beta sessions.json");

    h.spawn(&["--profile", "alpha"]);
    h.wait_for("[alpha]");
    h.assert_screen_contains("Alpha Session");
    h.assert_screen_not_contains("Beta Session");

    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("Down");
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.wait_for("[beta]");
    h.assert_screen_contains("Beta Session");
    h.assert_screen_not_contains("Alpha Session");

    let _home = HomeGuard::new(h.home_path());

    let svc: Arc<FileWatchService> = FileWatchService::noop();
    let storage = Storage::new("beta", svc).expect("storage for beta profile");
    let title = "filewatch-filtered-switch-row";
    storage
        .update(|i, _g| {
            let mut inst = Instance::new(title, "/tmp/filewatch-filtered-switch");
            inst.source_profile = "beta".to_string();
            i.push(inst);
            Ok(())
        })
        .expect("peer write to beta profile");

    h.wait_for_timeout(title, Duration::from_millis(1_500));
}

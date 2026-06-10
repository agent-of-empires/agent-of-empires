//! e2e: peer edits to a profile's `config.toml` must propagate through the
//! watcher path after an active-profile switch closes the cold-reload window.
//! These tests cover switch, delete/recreate, and create flows that rely on
//! `rewire_config_subscriptions` keeping the per-profile subscription set in sync.

use std::time::Duration;

use serial_test::serial;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[serial(file_watch)]
fn peer_edit_after_switch_propagates_via_watcher() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_profile_switch");

    let new_profile = "scratch_b";
    let config_dir = app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join(new_profile);
    std::fs::create_dir_all(&profile_dir).expect("seed profile B dir");

    h.enable_e2e_debug_signals();
    h.spawn(&["--profile", "default"]);
    h.wait_for(" aoe ");

    // Switch into B FIRST. switch_profile reads B's (empty) config; the
    // cold-reload path observes confirm_before_quit = false.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("Down");
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.assert_screen_contains("[scratch_b]");

    // Peer-write AFTER the switch. The cold-reload window already closed,
    // so only the watcher subscription installed by `rewire_config_subscriptions`
    // can deliver this change to the running TUI.
    let profile_b_config = profile_dir.join("config.toml");
    let baseline = h.read_watcher_config_refresh_count();
    std::fs::write(
        &profile_b_config,
        r#"[session]
confirm_before_quit = true
"#,
    )
    .expect("peer-write profile B config.toml");

    h.wait_for_watcher_config_refresh_above(baseline, Duration::from_secs(5));

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

/// Deleting and recreating a profile under the same name must leave exactly
/// one live config subscription for that profile so the next peer edit still
/// refreshes the running TUI.
#[test]
#[serial(file_watch)]
fn delete_then_recreate_same_name_propagates_via_watcher() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_recreate");
    let recycled = "scratch_recycle";
    let config_dir = app_dir_in(h.home_path());

    h.enable_e2e_debug_signals();
    h.spawn_tui();
    h.wait_for(" aoe ");

    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("n");
    h.wait_for("New Profile");
    h.type_text(recycled);
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.assert_screen_contains("[scratch_recycle]");

    let profile_dir = config_dir.join("profiles").join(recycled);
    std::fs::remove_dir_all(&profile_dir).expect("remove first incarnation");
    std::fs::create_dir_all(&profile_dir).expect("recreate same-name dir");

    let baseline = h.read_watcher_config_refresh_count();
    std::fs::write(
        profile_dir.join("config.toml"),
        r#"[session]
confirm_before_quit = true
"#,
    )
    .expect("peer-write recreated profile config.toml");

    // On macOS FSEvents coalesces attribute / metadata events with
    // multi-second latency before the watcher delivers; the 8 s bound
    // tolerates that worst case while the deterministic counter
    // signal gates the assertion.
    h.wait_for_watcher_config_refresh_above(baseline, Duration::from_secs(8));

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

#[test]
#[serial(file_watch)]
fn create_profile_then_peer_edit_propagates_via_watcher() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_profile_create");
    let new_profile = "scratch_create";
    let config_dir = app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join(new_profile);

    h.enable_e2e_debug_signals();
    h.spawn_tui();
    h.wait_for(" aoe ");

    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("n");
    h.wait_for("New Profile");
    h.type_text(new_profile);
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.assert_screen_contains("[scratch_create]");

    let baseline = h.read_watcher_config_refresh_count();
    std::fs::write(
        profile_dir.join("config.toml"),
        r#"[session]
confirm_before_quit = true
"#,
    )
    .expect("peer-write created profile config.toml");

    h.wait_for_watcher_config_refresh_above(baseline, Duration::from_secs(5));

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

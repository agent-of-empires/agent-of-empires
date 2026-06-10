//! e2e: profile removal tears down the per-profile config subscription
//! cleanly (drop-then-abort observed; no panics; TUI keeps running).
//!
//! Validates the profile-removal teardown rule and the canonical
//! drop-then-abort order for the config-watch path. The test:
//!
//! 1. Pre-seeds profile B alongside `default` so the TUI subscribes to
//!    its `config.toml` at startup (one subscription per known profile).
//! 2. Peer-writes B's `config.toml` and asserts no crash within the
//!    debounce window (the subscription is alive and feeding events).
//! 3. Removes profile B's directory, simulating a peer
//!    `aoe profile delete b`. The next disk-mirror tick rediscovers the
//!    profile set, calls `rewire_config_subscriptions`, and tears down
//!    B's entry (drop the `SubscriptionHandle`, then abort the
//!    forwarder).
//! 4. Asserts the TUI is still alive after the teardown window.

use std::time::Duration;

use serial_test::serial;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[serial(file_watch)]
fn profile_removal_tears_down_config_subscription_without_crash() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_profile_removal");

    let new_profile = "scratch_rm";
    let config_dir = app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join(new_profile);
    std::fs::create_dir_all(&profile_dir).expect("seed profile B dir");

    h.enable_e2e_debug_signals();
    h.spawn_tui();
    h.wait_for(" aoe ");

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
    assert!(
        h.session_alive(),
        "TUI must remain alive after peer-write to profile B config"
    );

    std::fs::remove_dir_all(&profile_dir).expect("remove profile B dir");

    // The disk-mirror tick fires every ~2s; wait for the picker to
    // reflect the removal as the deterministic "rewire teardown
    // completed" signal. Replaces a fixed 6s sleep that padded every
    // CI run while still flaking under load.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.wait_for_absent("scratch_rm", Duration::from_secs(8));
    assert!(
        h.session_alive(),
        "TUI must remain alive after profile B's directory is removed \
         (drop-then-abort teardown should not panic)"
    );
    h.send_keys("Escape");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
}

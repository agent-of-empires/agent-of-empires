//! Malformed config shows a reload error without crashing the TUI.
//! A valid peer edit clears the error and applies quit confirmation.

use std::time::Duration;

use serial_test::serial;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[serial(file_watch)]
fn malformed_then_valid_config_does_not_crash_and_recovers() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_malformed");
    h.enable_e2e_debug_signals();
    h.spawn_tui();
    h.wait_for(" aoe ");

    let config_dir = app_dir_in(h.home_path());
    let config_path = config_dir.join("config.toml");

    let baseline_malformed = h.read_watcher_config_refresh_count();
    std::fs::write(&config_path, b"[session\nconfirm_before_quit = true\n")
        .expect("peer-write malformed TOML");

    h.wait_for_watcher_config_refresh_above(baseline_malformed, Duration::from_secs(5));

    h.wait_for_timeout("Reload Failed", Duration::from_secs(5));

    assert!(
        h.session_alive(),
        "TUI must not crash on malformed config write"
    );
    h.assert_screen_contains(" aoe ");

    let valid = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
confirm_before_quit = true
"#,
        env!("CARGO_PKG_VERSION")
    );
    let baseline_recovery = h.read_watcher_config_refresh_count();
    std::fs::write(&config_path, valid).expect("peer-write valid TOML");

    h.wait_for_watcher_config_refresh_above(baseline_recovery, Duration::from_secs(5));
    h.wait_for_absent("Reload Failed", Duration::from_secs(5));

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

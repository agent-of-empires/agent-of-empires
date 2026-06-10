//! e2e: malformed TOML in `<app_dir>/config.toml` does not crash the TUI.
//!
//! `refresh_from_config()` routes through `resolve_config_or_warn` which
//! catches parse errors, logs a `target: "session.profile"` warning, and
//! returns `Config::default()`. The 100ms primitive debounce reduces but
//! does not eliminate the malformed-mid-edit window. The contract this
//! test verifies is the no-crash + recovery path:
//!
//! 1. With the TUI running, peer-write malformed TOML.
//! 2. Sleep past the debounce window.
//! 3. Assert the tmux session is still alive and " aoe " still renders.
//! 4. Peer-write valid TOML setting `confirm_before_quit = true`.
//! 5. Send `q`; assert the quit confirmation dialog appears (proves the
//!    consumer recovered from the bad-parse window and a subsequent
//!    valid edit propagated normally).

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

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

//! e2e: peer-process write to `<app_dir>/config.toml` propagates to the TUI
//! within sub-tick budget (1.5s typical, 3s ceiling on macOS).
//!
//! Strategy: the harness pre-seeds a config without
//! `confirm_before_quit`, so pressing `q` exits immediately. After the
//! TUI is up, we peer-write a config that adds
//! `[session] confirm_before_quit = true`. If the file-watch
//! subscription on the global config fires and `refresh_from_config()`
//! runs, the TUI's `confirm_before_quit` cache flips to `true` and the
//! next `q` shows the "Quit Agent of Empires" dialog instead of
//! exiting. That dialog title is the observable proof that the live
//! reload fired.

use std::time::Duration;

use serial_test::serial;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[serial(file_watch)]
fn external_config_edit_propagates_to_refresh_from_config() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_config_tui");
    h.enable_e2e_debug_signals();
    h.spawn_tui();
    h.wait_for(" aoe ");

    let config_dir = app_dir_in(h.home_path());
    let config_path = config_dir.join("config.toml");
    let updated = format!(
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
    let baseline = h.read_watcher_config_refresh_count();
    std::fs::write(&config_path, updated).expect("peer-write global config.toml");

    h.wait_for_watcher_config_refresh_above(baseline, Duration::from_secs(5));

    h.send_keys("q");
    h.wait_for_timeout("Quit Agent of Empires", Duration::from_secs(8));
    h.send_keys("Escape");
}

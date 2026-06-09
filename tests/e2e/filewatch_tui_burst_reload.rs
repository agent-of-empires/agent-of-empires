//! e2e: two back-to-back peer writes surface without waiting for the 5 s
//! heartbeat.
//!
//! The test runs two `Storage::update` calls against the same app dir the TUI
//! watches and asserts both rows appear within sub-tick budget. This proves
//! the watcher path keeps up with a small write burst; it does not try to
//! count exact reloads or enter live-send mode.

use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::file_watch::FileWatchService;
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[serial]
fn back_to_back_peer_writes_surface_within_sub_tick_budget() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_burst_reload");
    h.spawn_tui();
    h.wait_for(" aoe ");

    // SAFETY: env mutation; the harness owns its own isolated $HOME.
    // `#[serial]` guards cross-test races.
    unsafe { std::env::set_var("HOME", h.home_path()) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", h.home_path().join(".config"))
    };

    let svc: Arc<FileWatchService> = FileWatchService::noop();
    let storage = Storage::new("default", svc).expect("storage in test process");

    let first = "filewatch-live-row-a";
    let second = "filewatch-live-row-b";

    storage
        .update(|i, _g| {
            let mut inst = Instance::new(first, "/tmp/filewatch-a");
            inst.source_profile = "default".to_string();
            i.push(inst);
            Ok(())
        })
        .expect("first peer write");
    storage
        .update(|i, _g| {
            let mut inst = Instance::new(second, "/tmp/filewatch-b");
            inst.source_profile = "default".to_string();
            i.push(inst);
            Ok(())
        })
        .expect("second peer write");

    h.wait_for_timeout(first, Duration::from_millis(1_500));
    h.wait_for_timeout(second, Duration::from_millis(1_500));
}

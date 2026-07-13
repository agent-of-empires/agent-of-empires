//! e2e: peer-process write to `sessions.json` propagates to the TUI within
//! sub-tick budget (1.5 s, well under the 5 s heartbeat). Validates the
//! kernel-watcher path end-to-end through the production binary.
//!
//! The test launches the TUI under tmux, then performs a `Storage::update`
//! against the same profile dir from outside the TUI process (mimicking a
//! peer CLI invocation). With the subscription wired in `HomeView::new`,
//! the dirty flag flips, the tick consumes it, `reload_storage_only` runs,
//! and the new session row lands on screen within the harness's 1.5 s
//! timeout.

use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::file_watch::FileWatchService;
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;

use crate::harness::{require_tmux, HomeGuard, TuiTestHarness};

#[test]
#[serial]
fn peer_storage_update_reflects_within_sub_tick_budget() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_reload");
    h.spawn_tui();
    h.wait_for(" aoe ");

    // Set HOME/XDG_CONFIG_HOME for THIS process so `Storage::new`
    // resolves the same app dir the TUI is watching.
    let _home = HomeGuard::new(h.home_path());

    let svc: Arc<FileWatchService> = FileWatchService::noop();
    let storage = Storage::new("default", svc).expect("storage in test process");

    let title = "filewatch-test-row";
    storage
        .update(|i, _g| {
            let mut inst = Instance::new(title, "/tmp/filewatch-test");
            inst.source_profile = "default".to_string();
            i.push(inst);
            Ok(())
        })
        .expect("peer write to sessions.json");

    h.wait_for_timeout(title, Duration::from_millis(1_500));
}

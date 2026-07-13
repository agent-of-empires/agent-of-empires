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

use crate::harness::{require_tmux, TuiTestHarness};

/// RAII guard: points `HOME`/`XDG_CONFIG_HOME` at the harness's tempdir
/// for the test process and restores the prior values on `Drop`.
/// `#[serial]` on every caller linearizes this against other tests in
/// the binary; without the restore, a later test could inherit this
/// test's (by-then-dropped) tempdir path.
#[must_use = "HomeGuard restores env vars on Drop; bind it, don't discard it, or isolation ends immediately"]
struct HomeGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn new(home: &std::path::Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: env mutation; #[serial] linearizes this against every
        // other #[serial] test in the binary, so no concurrent
        // reader/writer exists.
        unsafe { std::env::set_var("HOME", home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        Self {
            prev_home,
            prev_xdg,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        fn restore_or_remove(key: &str, prev: Option<std::ffi::OsString>) {
            // SAFETY: same invariant as HomeGuard::new; #[serial] guards this.
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        restore_or_remove("HOME", self.prev_home.take());
        restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
    }
}

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

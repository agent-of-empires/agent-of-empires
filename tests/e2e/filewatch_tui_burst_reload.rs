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
fn back_to_back_peer_writes_surface_within_sub_tick_budget() {
    require_tmux!();

    let mut h = TuiTestHarness::new("filewatch_burst_reload");
    h.spawn_tui();
    h.wait_for(" aoe ");

    let _home = HomeGuard::new(h.home_path());

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

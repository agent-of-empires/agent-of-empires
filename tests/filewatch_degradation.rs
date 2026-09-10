//! `AOE_FILE_WATCH=off` returns a closed subscription without a kernel watcher.
//! This test needs its own process: the switch disables concurrent subscribers
//! regardless of their serial-test group.

use std::path::PathBuf;

use agent_of_empires::file_watch::{FileMatcher, WatchSpec};
use serial_test::serial;
use tempfile::TempDir;

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: this standalone binary serializes its environment mutations.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            // SAFETY: see `set` above.
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[tokio::test]
#[serial]
async fn aoe_file_watch_off_returns_noop_service() {
    let _guard = EnvGuard::set("AOE_FILE_WATCH", "off");
    let svc = agent_of_empires::file_watch::test_support::new_filewatch().expect("noop init");
    let tmp = TempDir::new().expect("tempdir");
    let target: PathBuf = tmp.path().join("watched");
    let (mut rx, _h) = svc
        .subscribe_channel(
            WatchSpec {
                dir: tmp.path().to_path_buf(),
                matcher: FileMatcher::Exact(target.clone()),
                debounce: None,
            },
            8,
        )
        .expect("subscribe must succeed on noop");

    std::fs::write(&target, "would-trigger-an-event").expect("write");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "noop subscriptions must already be closed, not merely silent"
    );
}

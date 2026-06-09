//! Graceful-degradation integration test for `AOE_FILE_WATCH=off`.
//!
//! With `AOE_FILE_WATCH=off`, `agent_of_empires::file_watch::test_support::new_filewatch()` must return the
//! noop fallback. Subscribers register but never receive events: the
//! dispatcher does not exist, the drain thread does not exist, and the
//! receiver returned by `subscribe_channel` is paired with a dropped
//! sender so `recv()` resolves to `None` immediately.
//!
//! `#[serial]` because we mutate the `AOE_FILE_WATCH` env var; running
//! in parallel with other env-touching tests would race.

use std::path::PathBuf;
use std::time::Duration;

use agent_of_empires::file_watch::{FileMatcher, WatchSpec};
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::timeout;

const NEG_WAIT: Duration = Duration::from_millis(300);

/// Set `AOE_FILE_WATCH` for the duration of the test, restoring the
/// previous value on drop. Wraps the unsafe env mutation in a single
/// place so the test body stays clear.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: 2024-edition `set_var` is unsafe because env is process
        // global and racy across threads. The `#[serial]` annotation
        // serialises this test against any other test that mutates env.
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

    // Real disk write: kernel events would normally fire here. With the
    // noop service there is no watcher, so nothing is delivered.
    std::fs::write(&target, "would-trigger-an-event").expect("write");

    // The paired sender was dropped before `subscribe_channel` returned;
    // `rx.recv()` resolves to `None` (channel closed) immediately. Either
    // outcome (`Ok(None)` or `Err(timeout)`) confirms no event delivery.
    let res = timeout(NEG_WAIT, rx.recv()).await;
    match res {
        Ok(None) => {}
        Err(_) => {}
        Ok(Some(ev)) => panic!("noop service must not deliver events, got {ev:?}"),
    }
}

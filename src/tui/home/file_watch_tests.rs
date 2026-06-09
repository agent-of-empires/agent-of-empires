//! In-module tests for `HomeView` file-watch wiring, exercising
//! `HomeView::new` and `HomeView::rewire_disk_subscriptions` directly.
//! The integration-level tests under `tests/filewatch_tui_*.rs`
//! exercise the same wiring against the public `file_watch` API in
//! isolation.
//!
//! Async TUI tests are segregated to this module so the much larger
//! synchronous `tests.rs` file is not forced to mix sync `#[test]`
//! with `#[tokio::test]` runtime infrastructure.

#![cfg(test)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use super::HomeView;
use crate::file_watch::FileWatchService;
use crate::session::{Instance, Storage};

fn isolate_home(temp: &std::path::Path) {
    // SAFETY: env mutation; #[serial] guards cross-test races on HOME.
    unsafe { std::env::set_var("HOME", temp) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"))
    };
}

/// Poll `pred` every 25ms up to `deadline`. Avoids a fixed sleep that
/// would either flake on slow CI or pad the test runtime on fast paths.
async fn wait_until<F>(deadline: Duration, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Locks the adapter-spawn contract: real watcher events must flip
/// `disk_dirty` through the `HomeView::new` wiring.
#[tokio::test]
#[serial]
async fn home_view_new_spawns_adapter_that_flips_disk_dirty() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-adapter").expect("seed dir");

    let view = HomeView::new(
        Some("hv-adapter".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    // Install a watcher subscription via the real rewire path so the
    // dispatcher routes peer writes through the adapter task that
    // HomeView::new just spawned.
    let mut view = view;
    view.rewire_disk_subscriptions(&["hv-adapter".to_string()])
        .expect("rewire");

    let writer = Storage::new("hv-adapter", live.clone()).expect("writer");
    writer
        .update(|i, _g| {
            *i = vec![Instance::new("peer-write", "/tmp/peer")];
            Ok(())
        })
        .expect("peer write");

    let flipped = wait_until(Duration::from_secs(2), || {
        view.disk_dirty.load(Ordering::Acquire)
    })
    .await;
    assert!(
        flipped,
        "HomeView::new must spawn the adapter task that flips disk_dirty on dispatcher events"
    );
}

/// Locks the canonical remove path in `rewire_disk_subscriptions`:
/// removing a profile must leave no stale `disk_watch_handles` entry
/// behind.
#[tokio::test]
#[serial]
async fn rewire_disk_subscriptions_drops_removed_profile_entry() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-keep").expect("dir");
    crate::session::get_profile_dir("hv-drop").expect("dir");

    let mut view = HomeView::new(
        Some("hv-keep".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-keep".to_string(), "hv-drop".to_string()])
        .expect("install both");
    assert!(
        view.disk_watch_handles.contains_key("hv-keep"),
        "precondition: hv-keep installed"
    );
    assert!(
        view.disk_watch_handles.contains_key("hv-drop"),
        "precondition: hv-drop installed"
    );

    view.rewire_disk_subscriptions(&["hv-keep".to_string()])
        .expect("remove hv-drop");

    assert!(
        view.disk_watch_handles.contains_key("hv-keep"),
        "rewire must keep entries for profiles still in the current set"
    );
    assert!(
        !view.disk_watch_handles.contains_key("hv-drop"),
        "rewire must drop+abort the entry for a removed profile"
    );
    assert_eq!(
        live.subscriber_count(),
        1,
        "exactly one live subscription remains after the removal"
    );
}

/// Locks the no-op fast-path invariant: when `rewire_disk_subscriptions`
/// is called with an unchanged profile set and no inode invalidation, the
/// fast-path returns without running the install loop, and a previously
/// latched `watcher_init_error` must be preserved (the install loop is
/// the only path that re-latches via `record_watcher_init_failure`).
#[tokio::test]
#[serial]
async fn rewire_no_op_preserves_latched_watcher_init_failure() {
    let temp = TempDir::new().expect("tempdir");
    isolate_home(temp.path());

    let live = FileWatchService::new().expect("live svc");
    crate::session::get_profile_dir("hv-noop").expect("seed dir");

    let mut view = HomeView::new(
        Some("hv-noop".to_string()),
        crate::tmux::AvailableTools::with_tools(&["claude"]),
        live.clone(),
    )
    .expect("HomeView::new");

    view.rewire_disk_subscriptions(&["hv-noop".to_string()])
        .expect("install");
    assert!(
        view.disk_watch_handles.contains_key("hv-noop"),
        "precondition: hv-noop installed"
    );

    view.reload_failure_state
        .record_watcher_init_failure("seed: simulated prior failure");
    assert!(
        view.reload_failure_state.has_any_failure(),
        "precondition: latch is set"
    );

    view.rewire_disk_subscriptions(&["hv-noop".to_string()])
        .expect("no-op rewire");

    assert!(
        view.reload_failure_state.has_any_failure(),
        "no-op rewire (unchanged set, no inode change) must preserve the watcher_init_error latch"
    );
}

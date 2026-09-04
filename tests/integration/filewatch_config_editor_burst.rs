//! Editor burst coalescing for config live-reload.
//!
//! Subscribes to `<dir>/config.toml` with the same spec
//! (`FileMatcher::Exact`, 100ms debounce) the TUI uses for the global
//! and per-profile config consumers. Then simulates a vim-style save:
//!
//! 1. Write a tempfile (`config.toml.tmp~`). The primitive's tempfile
//!    filter in `src/file_watch.rs` drops the event.
//! 2. Rename the tempfile to `config.toml`. A Modify event for the
//!    final path fires once content has landed.
//! 3. `chmod` `config.toml`. A second Modify event for the final path
//!    fires within microseconds of the rename.
//!
//! The 100ms debounce coalesces (2) and (3) into a single delivery.
//! End to end, exactly ONE event reaches the consumer side per logical
//! save, which means `refresh_from_config` runs exactly once per save.
//!
//! This is the primitive-level proof of the property; the e2e tests
//! cover the integration-level proof (TUI process, real watcher, real
//! tick loop).

use std::path::PathBuf;
use std::time::Duration;

use agent_of_empires::file_watch::{FileMatcher, FileWatchService, WatchSpec};
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::timeout;

const BURST_DEBOUNCE: Duration = Duration::from_millis(100);
const POST_BURST_QUIET: Duration = Duration::from_millis(800);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(file_watch)]
async fn vim_style_save_burst_collapses_to_a_single_delivery() {
    let svc = FileWatchService::new().expect("init service");
    let tmp = TempDir::new().expect("tempdir");
    let dir: PathBuf = tmp
        .path()
        .canonicalize()
        .expect("canonicalize tempdir (macOS resolves /var to /private/var)");
    let final_path = dir.join("config.toml");
    let temp_path = dir.join("config.toml.tmp~");

    let (mut rx, _handle) = svc
        .subscribe_channel(
            WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::Exact(final_path.clone()),
                debounce: Some(BURST_DEBOUNCE),
            },
            4,
        )
        .expect("subscribe_channel");

    std::fs::write(&final_path, b"theme = { idle_decay_minutes = 5 }\n")
        .expect("seed final_path so rename has something to overwrite");
    let first = timeout(Duration::from_millis(2_500), rx.recv())
        .await
        .expect("seed event arrives within 2.5s")
        .expect("seed event channel open");
    assert_eq!(
        first.path, final_path,
        "the seed write should match the spec's exact matcher"
    );

    while timeout(POST_BURST_QUIET, rx.recv()).await.is_ok() {}

    std::fs::write(&temp_path, b"theme = { idle_decay_minutes = 7 }\n")
        .expect("write tempfile (vim writebackup pattern)");
    std::fs::rename(&temp_path, &final_path).expect("rename tempfile to final path");
    // The chmod-burst portion of this test exercises rename + chmod
    // coalescing. macOS FSEvents collapses sibling attribute events
    // before they reach the dispatcher, so the second event needed to
    // exercise the debounce path on macOS would simply not fire from
    // chmod alone. Linux inotify delivers them as distinct events.
    // The sibling test `vim_style_save_via_two_renames_collapses` covers
    // the cross-platform shape using two distinct rename events.
    #[cfg(all(unix, target_os = "linux"))]
    {
        use std::os::unix::fs::PermissionsExt;
        // Flip the mode to a non-default value first so the second
        // chmod actually changes the on-disk metadata; setting 0o644
        // directly is often a no-op (write/rename already produced
        // 0o644 under umask 022) and would emit no event.
        let mut perms = std::fs::metadata(&final_path)
            .expect("stat final_path")
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&final_path, perms).expect("chmod 0o600");
        let mut perms = std::fs::metadata(&final_path)
            .expect("stat final_path")
            .permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&final_path, perms).expect("chmod 0o644");
    }

    let burst_event = timeout(Duration::from_millis(2_500), rx.recv())
        .await
        .expect("burst event arrives within 2.5s")
        .expect("burst event channel open");
    assert_eq!(
        burst_event.path, final_path,
        "burst event must target final config.toml"
    );

    let trailing = timeout(POST_BURST_QUIET, rx.recv()).await;
    assert!(
        trailing.is_err(),
        "100ms debounce must coalesce write + rename + chmod into a \
         single delivery; saw extra event {trailing:?}"
    );
}

/// Cross-platform sibling: two distinct rename events inside the
/// debounce window must coalesce into a single delivery. Renames fire
/// reliable events on every platform (including macOS FSEvents), so this
/// covers the same coalescing property as the chmod-burst test without
/// depending on platform-specific attribute-event semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(file_watch)]
async fn vim_style_save_via_two_renames_collapses_to_a_single_delivery() {
    let svc = FileWatchService::new().expect("init service");
    let tmp = TempDir::new().expect("tempdir");
    let dir: PathBuf = tmp
        .path()
        .canonicalize()
        .expect("canonicalize tempdir (macOS resolves /var to /private/var)");
    let final_path = dir.join("config.toml");
    let temp_a = dir.join("config.toml.tmp.a");
    let temp_b = dir.join("config.toml.tmp.b");

    let (mut rx, _handle) = svc
        .subscribe_channel(
            WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::Exact(final_path.clone()),
                debounce: Some(BURST_DEBOUNCE),
            },
            4,
        )
        .expect("subscribe_channel");

    std::fs::write(&final_path, b"theme = { idle_decay_minutes = 5 }\n")
        .expect("seed final_path so the first rename has something to overwrite");
    let _seed = timeout(Duration::from_millis(2_500), rx.recv())
        .await
        .expect("seed event arrives within 2.5s")
        .expect("seed event channel open");
    while timeout(POST_BURST_QUIET, rx.recv()).await.is_ok() {}

    std::fs::write(&temp_a, b"theme = { idle_decay_minutes = 7 }\n").expect("write temp_a");
    std::fs::rename(&temp_a, &final_path).expect("first rename onto final_path");
    std::fs::write(&temp_b, b"theme = { idle_decay_minutes = 11 }\n").expect("write temp_b");
    std::fs::rename(&temp_b, &final_path).expect("second rename onto final_path");

    let burst_event = timeout(Duration::from_millis(2_500), rx.recv())
        .await
        .expect("burst event arrives within 2.5s")
        .expect("burst event channel open");
    assert_eq!(
        burst_event.path, final_path,
        "burst event must target final config.toml"
    );

    let trailing = timeout(POST_BURST_QUIET, rx.recv()).await;
    assert!(
        trailing.is_err(),
        "100ms debounce must coalesce two back-to-back renames into a \
         single delivery; saw extra event {trailing:?}"
    );
}

//! Integration test for the server-consumer Local + Kernel propagation path.
//!
//! Local-Kernel best-effort coalescing: drive `Storage::update` from inside
//! the test process so both the in-process Local notify and the kernel echo
//! race the dispatcher; assert at least one delivery and no immediate
//! duplicate within a tight post-write budget. Slow backends may still emit
//! a later, idempotent kernel echo outside that short window.
//!
//! Full end-to-end coverage from `aoe serve` REST through the dispatcher
//! requires tunnel / port / auth setup beyond what's practical here. This
//! test verifies the in-process path:
//! Storage::update -> notify_local_change -> dispatcher Local arm ->
//! debounce-collapse with kernel echo -> subscriber receipt.

#![cfg(feature = "serve")]

use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::file_watch::{EventSource, FileMatcher, FileWatchService, WatchSpec};
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::timeout;

const KERNEL_WAIT: Duration = Duration::from_millis(2_500);
const NEG_WAIT: Duration = Duration::from_millis(300);

fn isolate_home(temp: &std::path::Path) {
    // SAFETY: env mutation; #[serial] guards cross-test races.
    unsafe { std::env::set_var("HOME", temp) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"))
    };
}

/// Storage::update fires `notify_local_change` after each successful
/// `atomic_write`. Subscribers wired to the same `FileWatchService`
/// must observe the write promptly. On fast backends the Local event and
/// kernel echo often share the active debounce slot; on slower backends a
/// late kernel echo can arrive later, but consumers remain idempotent.
#[tokio::test]
#[serial]
async fn storage_update_avoids_immediate_duplicate_delivery_after_local_notify() {
    let temp = TempDir::new().unwrap();
    isolate_home(temp.path());
    let svc: Arc<FileWatchService> =
        agent_of_empires::file_watch::test_support::new_filewatch().expect("init");
    let storage = Storage::new("propagation-test", svc.clone()).expect("storage");

    // Pre-create both files so canonicalize matches kernel paths from
    // the first dispatch. Subscribe AFTER seeding so the seed's kernel
    // events fire pre-subscribe and are not delivered.
    storage
        .update(|i, _g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })
        .expect("seed write");

    let profile_dir = agent_of_empires::session::get_profile_dir_path("propagation-test")
        .expect("resolve profile dir");
    let sessions_path = profile_dir.join("sessions.json");
    let groups_path = profile_dir.join("groups.json");

    let (mut rx, _h) = svc
        .subscribe_channel(
            WatchSpec {
                dir: profile_dir,
                matcher: FileMatcher::AnyOf(vec![sessions_path.clone(), groups_path]),
                debounce: Some(Duration::from_millis(75)),
            },
            16,
        )
        .expect("subscribe");

    // Storage::update issues notify_local_change AFTER atomic_write returns.
    // The kernel rename echo arrives ~ms later for the same canonical path.
    storage
        .update(|i, _g| {
            i.push(Instance::new("added", "/tmp/added"));
            Ok(())
        })
        .expect("update");

    // Exactly ONE delivery within the kernel-wait budget for sessions.json.
    // Local-first ordering at the dispatcher is locked by the unit test
    // `notify_local_change_delivers_local_first_and_tolerates_late_kernel_echo`
    // in `src/file_watch.rs`; this integration test only proves Local
    // propagation reaches the subscriber, not that it wins the race
    // against a fast kernel echo.
    let first = timeout(KERNEL_WAIT, rx.recv())
        .await
        .expect("at least one event")
        .expect("channel open");
    assert!(
        first.path.file_name().is_some_and(|n| n == "sessions.json"),
        "expected sessions.json event, got {:?}",
        first.path
    );

    // No immediate second delivery within a tight budget: fast backends
    // collapse the Local event and kernel echo into one slot. A slower
    // backend may still surface a later, idempotent kernel echo outside
    // this short window.
    let second = timeout(NEG_WAIT, rx.recv()).await;
    assert!(
        second.is_err() || matches!(second, Ok(None)),
        "Local + kernel echo for the same Storage::update must collapse to one delivery"
    );
}

/// `Storage::update` propagates a peer-process write through the kernel
/// path even when the in-process Local fast path is unavailable (noop).
/// Simulates the cross-process path: the writer holds a noop service so
/// `notify_local_change` is silent; the reader holds a live service whose
/// kernel watcher picks up the rename.
#[tokio::test]
#[serial]
async fn cross_process_kernel_path_delivers_when_local_is_noop() {
    let temp = TempDir::new().unwrap();
    isolate_home(temp.path());

    let writer_storage = Storage::new_unwatched("xproc-test").expect("writer");
    writer_storage
        .update(|i, _g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })
        .expect("seed");

    let profile_dir = agent_of_empires::session::get_profile_dir_path("xproc-test").expect("dir");
    let sessions_path = profile_dir.join("sessions.json");
    let groups_path = profile_dir.join("groups.json");

    let reader_svc: Arc<FileWatchService> =
        agent_of_empires::file_watch::test_support::new_filewatch().expect("reader init");
    let (mut rx, _h) = reader_svc
        .subscribe_channel(
            WatchSpec {
                dir: profile_dir,
                matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
                debounce: Some(Duration::from_millis(75)),
            },
            16,
        )
        .expect("subscribe");

    writer_storage
        .update(|i, _g| {
            i.push(Instance::new("peer", "/tmp/peer"));
            Ok(())
        })
        .expect("peer write");

    let ev = timeout(KERNEL_WAIT, rx.recv())
        .await
        .expect("kernel event arrives within budget")
        .expect("channel open");
    assert!(
        ev.path.file_name().is_some_and(|n| n == "sessions.json"),
        "expected sessions.json event, got {:?}",
        ev.path
    );
    assert_eq!(
        ev.source,
        EventSource::Kernel,
        "writer with noop service cannot reach the Local fast path; cross-process delivery must arrive via the kernel watcher"
    );
}

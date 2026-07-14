//! TUI file-watch forwarder lifetime regression.
//!
//! `HomeView::new` spawns a per-profile forwarder task for each disk-watch
//! subscription (sessions.json + groups.json). Each forwarder drains its
//! receiver and sets `disk_dirty.store(true, Release)` directly; there is
//! no intermediate adapter task or fan-in channel. This test exercises the
//! same primitive wiring against a tokio runtime, drives a `Storage::update`
//! write through the live `FileWatchService`, and asserts that the dirty
//! flag flips before the heartbeat budget. The regression it catches is
//! "forwarder task accidentally dropped on construct," which would close
//! the channel and leave the dirty flag forever stuck.

mod home_isolation;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_of_empires::file_watch::{FileMatcher, FileWatchService, WatchSpec};
use agent_of_empires::session::{Instance, Storage};
use home_isolation::isolate_home;
use serial_test::serial;
use tempfile::TempDir;

struct ForwarderHarness {
    disk_dirty: Arc<AtomicBool>,
    _handle: agent_of_empires::file_watch::SubscriptionHandle,
    _forwarder: tokio::task::AbortHandle,
}

async fn spawn_harness(svc: Arc<FileWatchService>, dir: PathBuf) -> ForwarderHarness {
    let sessions_path = dir.join("sessions.json");
    let groups_path = dir.join("groups.json");

    let (mut rx, handle) = svc
        .subscribe_channel(
            WatchSpec {
                dir,
                matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
                debounce: Some(Duration::from_millis(75)),
            },
            16,
        )
        .expect("subscribe_channel");

    let disk_dirty = Arc::new(AtomicBool::new(false));
    let forwarder_dirty = Arc::clone(&disk_dirty);
    let forwarder_join = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            forwarder_dirty.store(true, Ordering::Release);
        }
    });

    ForwarderHarness {
        disk_dirty,
        _handle: handle,
        _forwarder: forwarder_join.abort_handle(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn adapter_flips_disk_dirty_after_storage_update() {
    let temp = TempDir::new().unwrap();
    let _home = isolate_home(temp.path());

    let svc: Arc<FileWatchService> = FileWatchService::new().expect("init");

    let storage = Storage::new("adapter-lifetime", svc.clone()).expect("storage");
    storage
        .update(|i, _g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })
        .expect("seed write");

    let dir =
        agent_of_empires::session::get_profile_dir_path("adapter-lifetime").expect("profile dir");
    let harness = spawn_harness(svc, dir).await;

    storage
        .update(|i, _g| {
            i.push(Instance::new("after", "/tmp/after"));
            Ok(())
        })
        .expect("post-subscribe write");

    let deadline = Instant::now() + Duration::from_millis(2_500);
    let mut flipped = false;
    while Instant::now() < deadline {
        if harness.disk_dirty.load(Ordering::Acquire) {
            flipped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        flipped,
        "disk_dirty must flip to true after a Storage::update write to a watched profile"
    );
}

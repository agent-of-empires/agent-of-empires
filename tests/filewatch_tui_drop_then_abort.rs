//! Drop-then-abort canonical-order regression.
//!
//! Verifies the order in `HomeView::rewire_disk_subscriptions` for the
//! removed-profile branch: drop the `SubscriptionHandle` FIRST so the
//! source channel closes and the forwarder's `rx.recv().await` returns
//! `None` naturally, THEN call `forwarder.abort()` as a fast-path
//! safeguard. The wrong order (abort first) creates a brief window where
//! the forwarder could `try_send(())` against a still-arriving event,
//! racing the dispatcher's drop of the subscription.
//!
//! The regression assert: after `drop(handle)`, the forwarder task must
//! exit cleanly within a short deadline (its `recv().await` returns
//! `None` because the subscription's `DeliverySink` was deregistered).
//! No `try_send` runs after the drop point.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_of_empires::file_watch::{FileMatcher, FileWatchService, WatchSpec};
use agent_of_empires::session::{Instance, Storage};
use serial_test::serial;
use tempfile::TempDir;

fn isolate_home(temp: &std::path::Path) {
    // SAFETY: env mutation; #[serial] guards cross-test races.
    unsafe { std::env::set_var("HOME", temp) };
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"))
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn dropping_handle_closes_source_channel_before_forwarder_aborts() {
    let temp = TempDir::new().unwrap();
    isolate_home(temp.path());

    let svc: Arc<FileWatchService> = FileWatchService::new().expect("init");

    let storage = Storage::new("drop-then-abort", svc.clone()).expect("storage");
    storage
        .update(|i, _g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })
        .expect("seed write");
    let dir: PathBuf =
        agent_of_empires::session::get_profile_dir_path("drop-then-abort").expect("profile dir");
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

    let (combined_tx, mut combined_rx) = tokio::sync::mpsc::channel::<()>(1);

    let send_count = Arc::new(AtomicUsize::new(0));
    let forwarder_exited = Arc::new(AtomicBool::new(false));
    let send_count_for_task = Arc::clone(&send_count);
    let exited_for_task = Arc::clone(&forwarder_exited);
    let forwarder = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            send_count_for_task.fetch_add(1, Ordering::Release);
            match combined_tx.try_send(()) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => break,
            }
        }
        exited_for_task.store(true, Ordering::Release);
    });
    let forwarder_abort = forwarder.abort_handle();

    let drained = Arc::new(AtomicUsize::new(0));
    let drained_for_task = Arc::clone(&drained);
    tokio::spawn(async move {
        while combined_rx.recv().await.is_some() {
            drained_for_task.fetch_add(1, Ordering::Release);
        }
    });

    storage
        .update(|i, _g| {
            i.push(Instance::new("after", "/tmp/after"));
            Ok(())
        })
        .expect("post-subscribe write");

    let arm_deadline = Instant::now() + Duration::from_millis(2_500);
    while Instant::now() < arm_deadline {
        if send_count.load(Ordering::Acquire) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let pre_drop_sends = send_count.load(Ordering::Acquire);
    assert!(
        pre_drop_sends > 0,
        "arm phase must observe at least one event before drop; otherwise the test is vacuous"
    );
    drop(handle);

    let exit_deadline = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < exit_deadline {
        if forwarder_exited.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        forwarder_exited.load(Ordering::Acquire),
        "forwarder must exit cleanly after the SubscriptionHandle is dropped \
         (rx.recv() returns None because the dispatcher removes the sink)"
    );

    forwarder_abort.abort();

    storage
        .update(|i, _g| {
            i.push(Instance::new("post-drop", "/tmp/post-drop"));
            Ok(())
        })
        .expect("post-drop write");

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let post_drop_sends = send_count.load(Ordering::Acquire);
    assert_eq!(
        post_drop_sends, pre_drop_sends,
        "no events should reach the forwarder after the handle is dropped \
         (the dispatcher deregisters the sink synchronously inside Drop)"
    );
}

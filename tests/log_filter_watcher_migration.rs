//! Byte-identical-behavior integration test for the migrated
//! `watch_runtime_filter`.
//!
//! Verifies:
//! a) Apply-once-at-startup path: the file already exists when subscribe
//!    runs; the function calls `apply_filter_file` once before entering
//!    the receive loop.
//! b) Event path: a write after subscribe propagates through the kernel
//!    to the dispatcher to `apply_filter_file`.
//!
//! Both cases assert that `current_filter()` reflects the directive that
//! was persisted to `<tmpdir>/runtime_filter`. Includes a corrupt-content
//! pass to confirm we do not panic on garbage input.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_of_empires::file_watch::FileWatchService;
use agent_of_empires::logging;

const VALID_DIRECTIVE_A: &str = "agent_of_empires=info,hyper=warn";
const VALID_DIRECTIVE_B: &str = "agent_of_empires=trace,hyper=warn";
const TIMEOUT: Duration = Duration::from_secs(3);

/// Install a stdout-targeted subscriber once for this test process so we
/// have a real `FilterController` registered. `try_init` inside
/// `init_subscriber` is idempotent; subsequent calls quietly fail and we
/// reuse the controller installed by the first one.
fn install_subscriber_once(initial: &str) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let init = logging::init_subscriber(logging::SubscriberTarget::Stdout, initial.to_string());
        if let Some(c) = init.controller {
            logging::install_controller(c);
        }
    });
}

/// Spin until `current_filter()` matches `expected` or the deadline lapses.
async fn await_filter(expected: &str) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if logging::current_filter().as_deref() == Some(expected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn write_runtime_filter(app_dir: &std::path::Path, directive: &str) {
    let path = app_dir.join("runtime_filter");
    std::fs::write(&path, directive).expect("write runtime_filter");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_runtime_filter_byte_identical_behavior() {
    install_subscriber_once(VALID_DIRECTIVE_A);
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let app_dir: PathBuf = tmp.path().to_path_buf();

    // Case a: file pre-exists. Priming must call `apply_filter_file`
    // synchronously before the receive loop, so `current_filter` reflects
    // the directive almost immediately.
    write_runtime_filter(&app_dir, VALID_DIRECTIVE_A);

    let svc = FileWatchService::new().expect("init service");
    let watch_handle = tokio::spawn(logging::watch_runtime_filter(
        Arc::clone(&svc),
        app_dir.clone(),
    ));

    assert!(
        await_filter(VALID_DIRECTIVE_A).await,
        "apply-once-at-startup must propagate the pre-existing directive"
    );

    // Case b: write a different directive while the watcher is running.
    write_runtime_filter(&app_dir, VALID_DIRECTIVE_B);
    assert!(
        await_filter(VALID_DIRECTIVE_B).await,
        "kernel-driven event must propagate a post-subscribe write"
    );

    // Corrupt content: the function MUST NOT panic; current_filter stays
    // at the last valid value (apply_filter_file's `set_filter` rejects
    // garbage and emits a warn).
    write_runtime_filter(&app_dir, "<<<not a valid filter directive>>>");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        logging::current_filter().as_deref(),
        Some(VALID_DIRECTIVE_B),
        "corrupt directive must not displace the prior valid one"
    );

    // Recovery: a subsequent valid write succeeds.
    write_runtime_filter(&app_dir, VALID_DIRECTIVE_A);
    assert!(
        await_filter(VALID_DIRECTIVE_A).await,
        "watcher recovers after a corrupt write and applies the next valid one"
    );

    // Drop the watcher task: aborting drops the subscription handle, which
    // tears down the dispatcher's record for this consumer.
    watch_handle.abort();
    let _ = watch_handle.await;
}

//! Native config delivery after editor writes, renames, and permission changes.
//! Exact same-window coalescing is owned by
//! file_watch::tests::debounce_collapses_burst_to_one_event; native operations
//! may cross debounce windows when the producer is descheduled.

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
#[serial_test::parallel]
async fn vim_style_save_burst_delivers_final_config() {
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

    loop {
        match timeout(POST_BURST_QUIET, rx.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("seed event channel closed"),
            Err(_) => break,
        }
    }

    std::fs::write(&temp_path, b"theme = { idle_decay_minutes = 7 }\n")
        .expect("write tempfile (vim writebackup pattern)");
    std::fs::rename(&temp_path, &final_path).expect("rename tempfile to final path");
    // Linux emits attribute events separately; macOS may fold them into rename.
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

    assert_eq!(
        std::fs::read_to_string(&final_path).unwrap(),
        "theme = { idle_decay_minutes = 7 }\n"
    );
    // Exact same-window coalescing is covered by the dispatcher-clock test.
    while let Ok(event) = timeout(POST_BURST_QUIET, rx.recv()).await {
        assert_eq!(event.expect("watch channel open").path, final_path);
    }
}

/// Both native renames must leave the final config observable by the consumer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(file_watch)]
#[serial_test::parallel]
async fn vim_style_save_via_two_renames_delivers_final_config() {
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
    loop {
        match timeout(POST_BURST_QUIET, rx.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("seed event channel closed"),
            Err(_) => break,
        }
    }

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

    assert_eq!(
        std::fs::read_to_string(&final_path).unwrap(),
        "theme = { idle_decay_minutes = 11 }\n"
    );
    while let Ok(event) = timeout(POST_BURST_QUIET, rx.recv()).await {
        assert_eq!(event.expect("watch channel open").path, final_path);
    }
}

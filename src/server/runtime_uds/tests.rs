//! Publisher and admission behavior against owned temporary namespace directories.

use crate::process::runtime_io::{open_directory, open_lock_at, read_acl, set_acl, Umask};
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::*;

/// Own the temporary XDG base and its environment binding.
struct Namespace {
    base: tempfile::TempDir,
    _guard: crate::server::test_support::RuntimeEnvGuard,
}

impl Namespace {
    fn app_dir(&self) -> PathBuf {
        self.base.path().join(crate::session::APP_DIR_NAME_XDG)
    }
}

/// Client admission defines the producer/client compatibility boundary.
fn client_admits(path: &Path) -> bool {
    crate::cli::runtime_read::uds::open_trusted_directory(path, crate::process::effective_uid())
        .is_ok()
}

fn namespace() -> Option<Namespace> {
    let (base, _guard) = crate::server::test_support::trusted_namespace()?;
    Some(Namespace { base, _guard })
}

fn app_dir(namespace: &Namespace) -> PathBuf {
    let dir = namespace.app_dir();
    std::fs::create_dir_all(&dir).expect("app dir");
    dir
}

fn read_json(dir: &Path, name: &str) -> serde_json::Value {
    let path = dir.join(name);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", path.display()));
    serde_json::from_slice(&bytes).expect("marker json")
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// A marker a crashed daemon left behind: valid shape, dead process.
fn retained_marker(dir: &Path, name: &str) {
    let marker = serde_json::json!({
        "schema": SCHEMA,
        "pid": 1,
        "process_start_identity": "linux:v1:00000000-0000-0000-0000-000000000000:1",
        "prebind_instance_id": "11111111-2222-3333-4444-555555555555",
        "namespace": runtime_ws::NAMESPACE,
    });
    std::fs::write(dir.join(name), marker.to_string()).expect("write retained marker");
}

#[tokio::test]
#[serial_test::serial]
async fn namespace_discovery_waits_before_selecting_a_peers_temporary_parent() {
    use std::os::unix::fs::FileTypeExt;
    use std::sync::mpsc;
    use std::time::Duration;

    let mut held = crate::server::test_support::RuntimeEnvGuard::read_lock();
    let parent = tempfile::tempdir().expect("peer parent");
    let parent_path = parent.path().to_path_buf();
    held.bind(parent.path());
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let reactor = tokio::runtime::Handle::current();
    let reader = std::thread::spawn(move || {
        let _entered = reactor.enter();
        crate::test_env_lock::observe_env_lock_contention(waiting_tx);
        let (base, _env) = crate::server::test_support::trusted_namespace()
            .expect("a private ancestor chain exists");
        assert!(
            !base.path().starts_with(&parent_path),
            "discovery must not retain another fixture's temporary parent"
        );
        let _published = try_publish().expect("the owned namespace publishes");
        let socket = base
            .path()
            .join(crate::session::APP_DIR_NAME_XDG)
            .join(SOCKET_FILE);
        assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
        let _connected = std::os::unix::net::UnixStream::connect(socket)
            .expect("the actual published endpoint accepts a connection");
    });
    let contended = waiting_rx.recv_timeout(Duration::from_secs(30));
    parent
        .close()
        .expect("remove the peer parent before releasing its environment");
    drop(held);
    reader.join().expect("namespace reader completes");
    contended.expect("discovery must encounter the held process environment lock");
}

/// A live publication carries exactly the pair the client parses, at exactly
/// the modes and identities it checks.
#[tokio::test]
#[serial_test::serial]
async fn publication_writes_the_pair_the_client_parses() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let published = try_publish().expect("the namespace is free");

    let identity = runtime_ws::identity();
    let prebind = read_json(&dir, PREBIND_FILE);
    let postbind = read_json(&dir, POSTBIND_FILE);
    assert_eq!(prebind["schema"], SCHEMA);
    assert_eq!(prebind["pid"], std::process::id());
    assert_eq!(prebind["namespace"], runtime_ws::NAMESPACE);
    assert_eq!(prebind["prebind_instance_id"], identity.prebind_instance_id);
    assert_eq!(
        postbind["prebind_instance_id"],
        prebind["prebind_instance_id"]
    );
    assert_eq!(
        postbind["runtime_instance_id"],
        identity.runtime_instance_id
    );
    assert_eq!(postbind["runtime_epoch"], identity.runtime_epoch);
    assert_eq!(postbind["socket_path"], SOCKET_FILE);
    assert_eq!(postbind["owner_uid"], crate::process::effective_uid());

    // The recorded socket identity is the real one, not a placeholder.
    use std::os::unix::fs::MetadataExt;
    let socket = std::fs::metadata(dir.join(SOCKET_FILE)).expect("socket");
    assert_eq!(postbind["socket_device"], socket.dev());
    assert_eq!(postbind["socket_inode"], socket.ino());
    assert_eq!(postbind["socket_creator_pid"], std::process::id());

    assert_eq!(mode_of(&dir.join(LOCK_FILE)), 0o600);
    assert_eq!(mode_of(&dir.join(PREBIND_FILE)), 0o600);
    assert_eq!(mode_of(&dir.join(POSTBIND_FILE)), 0o600);
    assert_eq!(mode_of(&dir.join(SOCKET_FILE)), 0o600);
    assert!(mode_of(&dir) & 0o022 == 0, "the app dir must stay private");

    // A create-then-rename write leaves no temporary name behind.
    let directory = open_directory(&dir).expect("namespace directory");
    let leftovers: Vec<String> = directory_entries(directory.as_raw_fd())
        .expect("scan")
        .into_iter()
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "left temporaries: {leftovers:?}");
    drop(published);
}

/// A live publication is never replaced: a second daemon that finds the
/// namespace held refuses and leaves the artifacts byte-identical.
#[tokio::test]
#[serial_test::serial]
async fn a_live_publication_is_never_replaced() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let first = try_publish().expect("the namespace is free");
    let before = std::fs::read(dir.join(POSTBIND_FILE)).expect("postbind");

    let error = match try_publish() {
        Err(error) => error,
        Ok(_) => panic!("a live namespace cannot be republished"),
    };
    assert_eq!(error.code(), "namespace_busy");
    assert_eq!(
        std::fs::read(dir.join(POSTBIND_FILE)).expect("postbind"),
        before
    );
    assert!(dir.join(SOCKET_FILE).exists());
    drop(first);
}

#[tokio::test]
#[serial_test::serial]
async fn retained_dead_artifacts_are_reaped_before_publishing() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    retained_marker(&dir, PREBIND_FILE);
    retained_marker(&dir, POSTBIND_FILE);
    let stale = format!("{PREBIND_FILE}.tmp.11111111-2222-3333-4444-555555555555");
    retained_marker(&dir, &stale);

    // Torn: the marker a crash between the exclusive create and the rename
    // leaves, empty and therefore unparsable.
    let torn = format!("{POSTBIND_FILE}.tmp.99999999-8888-7777-6666-555555555555");
    std::fs::write(dir.join(&torn), b"").expect("torn temporary");
    // Half-written: a prefix of a real body, which does not parse either.
    let partial = format!("{PREBIND_FILE}.tmp.88888888-7777-6666-5555-444444444444");
    std::fs::write(dir.join(&partial), br#"{"schema":1,"pid":1"#).expect("partial temporary");

    let published = try_publish().expect("retained state is reapable");
    assert!(!dir.join(&stale).exists());
    assert!(!dir.join(&torn).exists(), "the torn temporary is reaped");
    assert!(
        !dir.join(&partial).exists(),
        "the partial temporary is reaped"
    );
    assert_eq!(
        read_json(&dir, POSTBIND_FILE)["prebind_instance_id"],
        runtime_ws::identity().prebind_instance_id
    );
    drop(published);
}

/// A temporary marker whose writer is still running means someone else is
/// publishing right now, so this daemon must not write anything at all.
#[tokio::test]
#[serial_test::serial]
async fn a_live_temporary_marker_stops_publication() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let identity = runtime_ws::identity();
    let temporary = format!("{POSTBIND_FILE}.tmp.{}", identity.prebind_instance_id);
    let marker = serde_json::json!({
        "schema": SCHEMA,
        "pid": std::process::id(),
        "process_start_identity": process_start_identity(std::process::id()).expect("start"),
        "prebind_instance_id": identity.prebind_instance_id,
        "runtime_instance_id": identity.runtime_instance_id,
        "runtime_epoch": identity.runtime_epoch,
        "namespace": runtime_ws::NAMESPACE,
        "socket_path": SOCKET_FILE,
        "owner_uid": crate::process::effective_uid(),
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": std::process::id(),
    });
    std::fs::write(dir.join(&temporary), marker.to_string()).expect("live temporary");

    let error = match try_publish() {
        Err(error) => error,
        Ok(_) => panic!("a live writer owns the namespace"),
    };
    assert_eq!(error.code(), "namespace_busy");
    assert!(
        dir.join(&temporary).exists(),
        "the live artifact is left alone"
    );
    assert!(!dir.join(PREBIND_FILE).exists(), "nothing was published");
}

#[tokio::test]
#[serial_test::serial]
async fn an_unreadable_proc_entry_refuses_publication_and_keeps_the_artifacts() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let identity = runtime_ws::identity();
    let marker = serde_json::json!({
        "schema": SCHEMA,
        "pid": std::process::id(),
        "process_start_identity": process_start_identity(std::process::id()).expect("start"),
        "prebind_instance_id": identity.prebind_instance_id,
        "runtime_instance_id": identity.runtime_instance_id,
        "runtime_epoch": identity.runtime_epoch,
        "namespace": runtime_ws::NAMESPACE,
        "socket_path": SOCKET_FILE,
        "owner_uid": crate::process::effective_uid(),
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": std::process::id(),
    });
    let before = marker.to_string();
    std::fs::write(dir.join(POSTBIND_FILE), &before).expect("retained postbind");

    fail_next_proc_read();
    let error = match try_publish() {
        Err(error) => error,
        Ok(_) => panic!("an unprovable process must not be reaped"),
    };
    assert_eq!(error.code(), "namespace_busy");
    assert_eq!(
        std::fs::read(dir.join(POSTBIND_FILE)).expect("postbind"),
        before.as_bytes(),
        "the retained marker must survive a refused publication"
    );
    assert!(!dir.join(PREBIND_FILE).exists(), "nothing was published");
}

/// Unreadable process identity preserves retained temporary artifacts.
#[tokio::test]
#[serial_test::serial]
async fn an_unreadable_proc_entry_keeps_a_retained_temporary_marker_too() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let marker = serde_json::json!({
        "schema": SCHEMA,
        "pid": std::process::id(),
        "process_start_identity": process_start_identity(std::process::id()).expect("start"),
        "prebind_instance_id": runtime_ws::identity().prebind_instance_id,
        "runtime_instance_id": runtime_ws::identity().runtime_instance_id,
        "runtime_epoch": runtime_ws::identity().runtime_epoch,
        "namespace": runtime_ws::NAMESPACE,
        "socket_path": SOCKET_FILE,
        "owner_uid": crate::process::effective_uid(),
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": std::process::id(),
    });
    let before = marker.to_string();
    let name = format!("{POSTBIND_FILE}.tmp.44444444-3333-2222-1111-000000000000");
    let temporary = dir.join(&name);
    std::fs::write(&temporary, &before).expect("retained temporary");

    fail_next_proc_read();
    let error = match try_publish() {
        Err(error) => error,
        Ok(_) => panic!("an unprovable process must not have its temporary reaped"),
    };
    assert_eq!(error.code(), "namespace_busy");
    assert_eq!(
        std::fs::read(&temporary).expect("temporary"),
        before.as_bytes(),
        "the retained temporary must survive a refused publication"
    );
    assert!(!dir.join(PREBIND_FILE).exists(), "nothing was published");
}

#[tokio::test]
#[serial_test::serial]
async fn a_torn_temporary_is_reaped_because_no_writer_is_left_to_honour() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let name = format!("{POSTBIND_FILE}.tmp.55555555-4444-3333-2222-111111111111");
    let temporary = dir.join(&name);
    std::fs::write(&temporary, b"{\"schema\":1,\"pid\":").expect("retained temporary");

    try_publish().expect("a torn temporary must not refuse publication forever");
    assert!(!temporary.exists(), "the torn temporary is reaped");
}

#[test]
#[serial_test::serial]
fn a_foreign_schema_marker_is_reaped_only_from_a_writer_proven_dead() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let pid = std::process::id();
    let live = process_start_identity(pid).expect("this process has a start identity");
    // (schema, identity, the code publication is refused with; `None` is the
    // retained state a new daemon is expected to reap)
    let cases: [(u8, String, Option<&str>); 4] = [
        // Another boot ended, so the writer is gone whatever its body says.
        (
            SCHEMA + 1,
            "linux:v1:00000000-0000-0000-0000-000000000000:1".to_string(),
            None,
        ),
        // An identity this half cannot place proves nothing at all.
        (
            SCHEMA + 1,
            format!("darwin:v1:{}:1", boot_id().expect("boot id")),
            Some("marker_foreign"),
        ),
        // A writer this process can see is publishing right now.
        (SCHEMA + 1, live.clone(), Some("marker_foreign")),
        // The same writer on a schema this half does speak: still refused, but
        // as a busy namespace rather than as a foreign body.
        (SCHEMA, live.clone(), Some("namespace_busy")),
    ];
    for (schema, identity, refused) in cases {
        let path = dir.join(PREBIND_FILE);
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": schema,
                "pid": pid,
                "process_start_identity": identity,
                "prebind_instance_id": "11111111-2222-3333-4444-555555555555",
            })
            .to_string(),
        )
        .expect("retained marker");

        let opened = open_trusted_app_dir(&dir).expect("open the namespace directory");
        let reaped = reap_retained_state(opened.as_raw_fd());
        match refused {
            Some(code) => {
                let error = reaped.expect_err("a writer this half cannot place owns the namespace");
                assert_eq!(error.code(), code, "{identity} is refused as {code}");
                assert!(
                    path.exists(),
                    "{identity} must be left in place, not reaped"
                );
                std::fs::remove_file(&path).expect("clear the refused artifact");
            }
            None => {
                reaped.expect("a proven dead writer's artifact is retained state");
                assert!(
                    !path.exists(),
                    "a dead writer's foreign marker is reaped like any other"
                );
            }
        }
    }
}

/// Shutdown removes this daemon's artifacts, and only those.
#[tokio::test]
#[serial_test::serial]
async fn shutdown_retracts_only_its_own_publication() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    drop(try_publish().expect("the namespace is free"));
    for name in [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE] {
        assert!(!dir.join(name).exists(), "{name} survived shutdown");
    }
    assert!(
        dir.join(LOCK_FILE).exists(),
        "the lock file outlives every publication"
    );
}

/// When another publication has taken over the pair, this daemon's shutdown
/// leaves it intact: retraction is proven by content, not by path.
#[tokio::test]
#[serial_test::serial]
async fn shutdown_leaves_a_foreign_publication_in_place() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let mut published = try_publish().expect("the namespace is free");
    let foreign = serde_json::json!({
        "schema": SCHEMA,
        "pid": 1,
        "process_start_identity": "linux:v1:00000000-0000-0000-0000-000000000000:1",
        "prebind_instance_id": "99999999-8888-7777-6666-555555555555",
        "runtime_instance_id": "99999999-8888-7777-6666-555555555555",
        "runtime_epoch": "99999999-8888-7777-6666-555555555555",
        "namespace": runtime_ws::NAMESPACE,
        "socket_path": SOCKET_FILE,
        "owner_uid": crate::process::effective_uid(),
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": 1,
    });
    std::fs::write(dir.join(POSTBIND_FILE), foreign.to_string()).expect("foreign postbind");

    published.retract().expect("retraction is a no-op here");
    assert!(dir.join(POSTBIND_FILE).exists());
    assert!(dir.join(SOCKET_FILE).exists());
}

/// An absence observer delays a successor; only an exclusive publisher refuses it.
#[tokio::test]
#[serial_test::serial]
async fn a_shared_absence_probe_cannot_disable_successor_publication() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let directory = open_directory(&dir).expect("namespace directory");
    let probe = open_lock(directory.as_raw_fd(), PUBLISHER_LOCK_FILE).unwrap();
    assert!(lock_shared(&probe));
    let cancelled = tokio_util::sync::CancellationToken::new();
    let publishing = publish_when_available(&cancelled);
    tokio::pin!(publishing);
    assert!(matches!(
        std::future::poll_fn(|context| {
            std::task::Poll::Ready(std::future::Future::poll(publishing.as_mut(), context))
        })
        .await,
        std::task::Poll::Pending
    ));
    cancelled.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), publishing)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert!(!dir.join(SOCKET_FILE).exists());

    let shutdown = tokio_util::sync::CancellationToken::new();
    let publishing = publish_when_available(&shutdown);
    tokio::pin!(publishing);
    assert!(matches!(
        std::future::poll_fn(|context| {
            std::task::Poll::Ready(std::future::Future::poll(publishing.as_mut(), context))
        })
        .await,
        std::task::Poll::Pending
    ));
    fs2::FileExt::unlock(&probe).unwrap();
    let _published = tokio::time::timeout(std::time::Duration::from_secs(5), publishing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(dir.join(SOCKET_FILE).exists());
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            publish_when_available(&shutdown)
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .code(),
        "namespace_busy"
    );
}

/// The daemon holds the namespace lock shared, so a client still takes it while
/// a second publisher's exclusive request is refused.
#[tokio::test]
#[serial_test::serial]
async fn the_held_lock_admits_a_client_and_refuses_a_publisher() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let _published = try_publish().expect("the namespace is free");
    let parent = open_directory(&dir).expect("namespace directory");
    let client = open_lock_at(parent.as_raw_fd(), &CString::new(LOCK_FILE).unwrap())
        .expect("client lock descriptor");
    assert!(lock_shared(&client), "a client must be able to lock");
    assert!(
        !lock_exclusive(&client).unwrap(),
        "no second publisher may lock"
    );
    unlock(&client);
}

#[test]
#[serial_test::serial]
fn process_identity_distinguishes_live_from_retained() {
    let pid = std::process::id();
    let live = MarkerProbe {
        schema: SCHEMA,
        pid,
        process_start_identity: process_start_identity(pid)
            .expect("this process has a start identity"),
        prebind_instance_id: "11111111-2222-3333-4444-555555555555".to_string(),
    };
    assert_eq!(process_liveness(&live), ProcessLiveness::Live);
    assert_eq!(
        live.process_start_identity,
        format!(
            "linux:v1:{}:{}",
            boot_id().expect("boot id"),
            match process_start_ticks(pid) {
                ProcessStart::Ticks(ticks) => ticks,
                _ => panic!("this process's /proc entry must be readable"),
            }
        )
    );

    let recycled = MarkerProbe {
        process_start_identity: format!("linux:v1:{}:0", boot_id().expect("boot id")),
        ..clone_probe(&live)
    };
    assert_eq!(
        process_liveness(&recycled),
        ProcessLiveness::Dead,
        "a different start time is dead"
    );

    let rebooted = MarkerProbe {
        process_start_identity: "linux:v1:00000000-0000-0000-0000-000000000000:1".to_string(),
        ..clone_probe(&live)
    };
    assert_eq!(
        process_liveness(&rebooted),
        ProcessLiveness::Dead,
        "another boot is dead"
    );

    let boot = boot_id().expect("boot id");
    for identity in [
        format!("darwin:v1:{boot}:1"),
        format!("linux:v2:{boot}:1"),
        format!("linux:v1:{boot}"),
        String::new(),
    ] {
        let unprovable = MarkerProbe {
            process_start_identity: identity.clone(),
            ..clone_probe(&live)
        };
        assert_eq!(
            process_liveness(&unprovable),
            ProcessLiveness::Unprovable,
            "{identity:?} is unprovable, not an absence"
        );
    }

    fail_next_proc_read();
    assert_eq!(
        process_liveness(&live),
        ProcessLiveness::Unprovable,
        "a well-formed identity over an unreadable /proc is unprovable"
    );
}

#[test]
#[serial_test::serial]
fn a_marker_this_half_cannot_prove_dead_is_never_reaped() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let boot = boot_id().expect("boot id");
    for identity in [
        format!("darwin:v1:{boot}:1"),
        format!("linux:v2:{boot}:1"),
        format!("linux:v1:{boot}"),
        String::new(),
    ] {
        let path = dir.join(PREBIND_FILE);
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": SCHEMA,
                "pid": std::process::id(),
                "process_start_identity": identity,
                "prebind_instance_id": "11111111-2222-3333-4444-555555555555",
            })
            .to_string(),
        )
        .expect("write the retained marker");

        let opened = open_trusted_app_dir(&dir).expect("open the namespace directory");
        let refused = reap_retained_state(opened.as_raw_fd());
        assert!(refused.is_err(), "{identity:?} must refuse publication");
        assert!(
            path.exists(),
            "{identity:?} must be left in place, not reaped"
        );
    }
}

fn clone_probe(probe: &MarkerProbe) -> MarkerProbe {
    MarkerProbe {
        schema: probe.schema,
        pid: probe.pid,
        process_start_identity: probe.process_start_identity.clone(),
        prebind_instance_id: probe.prebind_instance_id.clone(),
    }
}

/// The publisher only adopts a chain the client would also accept.
#[test]
#[serial_test::serial]
fn the_publisher_only_publishes_into_a_trusted_chain() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    assert!(client_admits(&app_dir(&namespace)));

    let widened = namespace.base.path().join("widened");
    std::fs::create_dir(&widened).expect("widened");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&widened, std::fs::Permissions::from_mode(0o770)).expect("widen");
    assert!(!client_admits(&widened));
    assert_eq!(
        open_trusted_app_dir(&widened)
            .err()
            .map(|error| error.code()),
        Some("app_dir_untrusted"),
        "a group-writable directory is never adopted"
    );
}

#[test]
#[serial_test::serial]
fn a_symlinked_prefix_resolves_to_the_directory_it_verified() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let real = namespace.base.path().join("real");
    let app = real.join(crate::session::APP_DIR_NAME_XDG);
    std::fs::create_dir_all(&app).expect("app dir under the real prefix");
    let link = namespace.base.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("prefix symlink");

    let through_the_link = link.join(crate::session::APP_DIR_NAME_XDG);
    let dir = open_trusted_app_dir(&through_the_link).expect("a symlinked prefix is followed");
    let expected = std::fs::symlink_metadata(&through_the_link).expect("the resolved directory");
    let stat = std::fs::metadata(format!("/proc/self/fd/{}", dir.as_raw_fd())).expect("fd stat");
    assert_eq!(
        (stat.dev(), stat.ino()),
        (expected.dev(), expected.ino()),
        "the returned descriptor must be the directory the walk verified"
    );
}

#[test]
#[serial_test::serial]
fn a_named_user_write_acl_is_refused_on_an_intermediate_component() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let intermediate = namespace.base.path().join("intermediate");
    let app = intermediate.join(crate::session::APP_DIR_NAME_XDG);
    std::fs::create_dir_all(&app).expect("app dir under the intermediate prefix");
    assert!(
        client_admits(&app),
        "the chain is admitted before the ACL lands"
    );

    let other = crate::process::effective_uid().wrapping_add(1);
    let value = acl_v2(0o6, other);
    set_acl(&intermediate, &value);
    assert_eq!(
        read_acl(&intermediate),
        value,
        "the walk must see the access ACL that was set, byte for byte"
    );
    assert_eq!(
        stat_mode(&intermediate),
        0o740,
        "the mode gate is not what refuses this directory"
    );

    assert_eq!(
        open_trusted_app_dir(&app).err().map(|error| error.code()),
        Some("app_dir_untrusted"),
        "a named-user write ACL is refused wherever it appears in the chain"
    );
    let refusal = client_refusal(&app);
    assert!(
        refusal.contains(&intermediate.display().to_string()),
        "the refusal names the component it refused: {refusal}"
    );
    assert!(
        refusal.contains("0740"),
        "the refusal carries the mode read off the descriptor: {refusal}"
    );
    assert!(
        refusal.contains("grants a named user or group write access"),
        "the refusal names the ACL cause, not some other one: {refusal}"
    );
}

#[test]
#[serial_test::serial]
fn a_named_user_readonly_acl_is_admitted_by_both_walks() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let intermediate = namespace.base.path().join("readonly");
    let app = intermediate.join(crate::session::APP_DIR_NAME_XDG);
    std::fs::create_dir_all(&app).expect("app dir under the intermediate prefix");
    assert!(
        client_admits(&app),
        "the chain is admitted before the ACL lands"
    );

    let other = crate::process::effective_uid().wrapping_add(1);
    let value = acl_v2(0o4, other);
    set_acl(&intermediate, &value);
    assert_eq!(
        read_acl(&intermediate),
        value,
        "the walk must see the access ACL that was set, byte for byte"
    );
    assert_eq!(
        stat_mode(&intermediate),
        0o740,
        "the two lanes differ only in the named entry's permissions"
    );

    assert!(client_admits(&app), "a named read is not a named write");
    assert!(
        open_trusted_app_dir(&app).is_ok(),
        "the producer admits what the client admits"
    );
}

/// Linux access ACLs encode a little-endian version followed by eight-byte entries.
const ACL_VERSION: u32 = 2;
const ACL_ENTRY_LEN: usize = 8;
const ACL_USER_OBJ: u8 = 0x01;
const ACL_USER: u8 = 0x02;
const ACL_GROUP_OBJ: u8 = 0x04;
const ACL_MASK: u8 = 0x10;
const ACL_OTHER: u8 = 0x20;

/// The id a base entry carries, and the only one the kernel accepts for them.
const ACL_UNDEFINED_ID: u32 = u32::MAX;

/// Encode an access ACL in kernel-required entry order.
fn acl_v2(named_permissions: u8, uid: u32) -> Vec<u8> {
    let mut value = ACL_VERSION.to_le_bytes().to_vec();
    for (tag, permissions) in [
        (ACL_USER_OBJ, 0o7u8),
        (ACL_USER, named_permissions),
        (ACL_GROUP_OBJ, 0),
        (ACL_MASK, 0o4),
        (ACL_OTHER, 0),
    ] {
        value.extend_from_slice(&u16::from(tag).to_le_bytes());
        value.extend_from_slice(&u16::from(permissions).to_le_bytes());
        let id = if tag == ACL_USER {
            uid
        } else {
            ACL_UNDEFINED_ID
        };
        value.extend_from_slice(&id.to_le_bytes());
    }
    assert_eq!(value.len(), 4 + 5 * ACL_ENTRY_LEN);
    value
}

/// The mode bits the walk judges, as `fstat` reports them.
fn stat_mode(dir: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).expect("stat").mode() & 0o7777
}

/// Preserve the client admission cause for the boundary assertion.
fn client_refusal(path: &Path) -> String {
    let walked = crate::cli::runtime_read::uds::open_trusted_directory(
        path,
        crate::process::effective_uid(),
    );
    format!("{walked:?}")
}

/// Restrictive umasks must not prevent marker reads or owned retraction.
#[tokio::test]
#[serial_test::serial]
async fn a_marker_keeps_its_mode_under_a_umask_that_would_clear_it() {
    const CHILD: &str = "AOE_UDS_UMASK_TEST_CHILD";
    const ENTERED: &str = "AOE_UDS_UMASK_TEST_ENTERED";
    let _env = crate::session::test_support::EnvGuard::read_lock();
    let thread = std::thread::current();
    let test = thread.name().expect("named test thread");
    if std::env::var(CHILD).as_deref() != Ok(test) {
        let home = tempfile::tempdir().expect("private child home");
        let entered = home.path().join("entered");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD, test)
            .env(ENTERED, &entered)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("TMPDIR", home.path())
            .stdin(std::process::Stdio::null());
        let output = crate::process::run_with_timeout_process_group(
            &mut command,
            std::time::Duration::from_secs(60),
        )
        .expect("spawn isolated umask test")
        .expect("isolated umask test timed out");
        assert!(
            output.status.success(),
            "isolated umask test failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(entered).unwrap(), test);
        return;
    }
    std::fs::write(std::env::var_os(ENTERED).expect("child entry path"), test)
        .expect("acknowledge selected test");
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    {
        let _umask = Umask::set(0o777);
        let published = try_publish().expect("the namespace is free under the mask");
        assert_eq!(
            mode_of(&dir.join(PREBIND_FILE)),
            0o600,
            "the prebind marker is owner read and write whatever the umask says"
        );
        assert_eq!(
            mode_of(&dir.join(POSTBIND_FILE)),
            0o600,
            "the postbind marker is owner read and write whatever the umask says"
        );
        // Retraction must still read owned markers under the restrictive mask.
        drop(published);
        // Lock files remain for future publishers.
        for name in [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE] {
            assert!(
                !dir.join(name).exists(),
                "{name} must be gone after retraction, or the namespace is wedged"
            );
        }
        let republished = try_publish().expect("a retracted namespace is publishable");
        drop(republished);
    }
    // Directory admission remains valid after publication is retracted.
    assert!(
        client_admits(&dir),
        "the client admits the published directory: {}",
        client_refusal(&dir)
    );
}

#[test]
#[serial_test::serial]
fn a_symlinked_final_component_is_still_refused() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let real = namespace.base.path().join("real");
    std::fs::create_dir_all(&real).expect("real dir");
    let link = namespace.base.path().join(crate::session::APP_DIR_NAME_XDG);
    std::os::unix::fs::symlink(&real, &link).expect("final symlink");
    assert_eq!(
        open_trusted_app_dir(&link).err().map(|error| error.code()),
        Some("app_dir_untrusted"),
        "a symlinked app directory is never adopted"
    );
}

#[test]
#[serial_test::serial]
fn a_symlinked_prefix_to_a_world_writable_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let open_dir = namespace.base.path().join("open");
    std::fs::create_dir_all(&open_dir).expect("open dir");
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o700))
        .expect("private prefix");
    let app = open_dir.join(crate::session::APP_DIR_NAME_XDG);
    std::fs::create_dir(&app).expect("app dir");
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).expect("private app");
    let link = namespace.base.path().join("elsewhere");
    std::os::unix::fs::symlink(&open_dir, &link).expect("prefix symlink");
    let aliased_app = link.join(crate::session::APP_DIR_NAME_XDG);
    assert!(open_trusted_app_dir(&aliased_app).is_ok());

    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).expect("widen");
    assert_eq!(
        open_trusted_app_dir(&aliased_app)
            .err()
            .map(|error| error.code()),
        Some("app_dir_untrusted"),
    );
}

/// Retained readers do not prevent retraction of the owned publication.
#[tokio::test]
#[serial_test::serial]
async fn retraction_succeeds_while_a_client_holds_the_namespace() {
    let namespace = namespace().expect("a private ancestor chain exists on this host");
    let dir = app_dir(&namespace);
    let mut published = try_publish().expect("the namespace is free");
    let parent = open_directory(&dir).expect("namespace directory");
    let client = open_lock_at(parent.as_raw_fd(), &CString::new(LOCK_FILE).unwrap())
        .expect("client lock descriptor");
    assert!(lock_shared(&client), "a client must be able to lock");

    published
        .retract()
        .expect("retraction under a held shared lock");
    for name in [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE] {
        assert!(
            !dir.join(name).exists(),
            "{} must be gone after retraction",
            name
        );
    }
    unlock(&client);
}

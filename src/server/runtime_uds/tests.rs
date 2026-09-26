//! Publisher behavior against a real namespace directory.
//!
//! Every test publishes into its own temporary app dir under a base whose whole
//! ancestor chain satisfies the client's trusted walk, so nothing here touches
//! the developer's real app directory.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

use super::*;

/// A private temporary XDG base plus the environment binding that points the
/// app dir, and therefore both the publisher and the client, at it.
struct Namespace {
    base: tempfile::TempDir,
    _guard: crate::session::test_support::RuntimeAppDirGuard,
}

impl Namespace {
    fn app_dir(&self) -> PathBuf {
        self.base.path().join(crate::session::APP_DIR_NAME_XDG)
    }
}

/// The first base whose whole ancestor chain is private enough for the client's
/// walk. `None` means this host has no such directory, which is reported rather
/// than silently passed.
fn namespace() -> Option<Namespace> {
    let mut candidates = Vec::new();
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        candidates.push(base);
    }
    candidates.push(std::env::temp_dir());
    if let Some(home) = dirs::home_dir() {
        candidates.push(home);
    }
    for base in candidates {
        if !base.is_absolute() || !trusted_chain(&base) {
            continue;
        }
        let Ok(base) = tempfile::tempdir_in(&base) else {
            continue;
        };
        let guard = crate::session::test_support::RuntimeAppDirGuard::set(
            &base.path().join(crate::session::APP_DIR_NAME_XDG),
        );
        return Some(Namespace {
            base,
            _guard: guard,
        });
    }
    None
}

fn namespace_or_skip() -> Option<Namespace> {
    match namespace() {
        Some(namespace) => Some(namespace),
        None => {
            eprintln!("skipping: no private ancestor chain exists for the trusted app dir walk");
            None
        }
    }
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

/// A live publication carries exactly the pair the client parses, at exactly
/// the modes and identities it checks.
#[tokio::test]
#[serial_test::serial]
async fn publication_writes_the_pair_the_client_parses() {
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    let dir = app_dir(&namespace);
    let published = publish().expect("the namespace is free");

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
    assert_eq!(postbind["owner_uid"], unsafe { libc::geteuid() });

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
    let leftovers: Vec<String> = directory_entries(open_dir(&dir).expect("app dir fd").as_raw_fd())
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
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    let dir = app_dir(&namespace);
    let first = publish().expect("the namespace is free");
    let before = std::fs::read(dir.join(POSTBIND_FILE)).expect("postbind");

    let error = match publish() {
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

/// Retained crash state from a dead process is reaped, so the new publication
/// lands where the client would otherwise fail closed. A crash between the
/// exclusive create and the rename leaves a body that does not parse, and that
/// body must not decide whether this publication may happen: the client refuses
/// the very file, so the publisher has to remove it first.
#[tokio::test]
#[serial_test::serial]
async fn retained_dead_artifacts_are_reaped_before_publishing() {
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
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

    let published = publish().expect("retained state is reapable");
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
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
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
        "owner_uid": unsafe { libc::geteuid() },
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": std::process::id(),
    });
    std::fs::write(dir.join(&temporary), marker.to_string()).expect("live temporary");

    let error = match publish() {
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

/// Shutdown removes this daemon's artifacts, and only those.
#[tokio::test]
#[serial_test::serial]
async fn shutdown_retracts_only_its_own_publication() {
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    let dir = app_dir(&namespace);
    drop(publish().expect("the namespace is free"));
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
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    let dir = app_dir(&namespace);
    let mut published = publish().expect("the namespace is free");
    let foreign = serde_json::json!({
        "schema": SCHEMA,
        "pid": 1,
        "process_start_identity": "linux:v1:00000000-0000-0000-0000-000000000000:1",
        "prebind_instance_id": "99999999-8888-7777-6666-555555555555",
        "runtime_instance_id": "99999999-8888-7777-6666-555555555555",
        "runtime_epoch": "99999999-8888-7777-6666-555555555555",
        "namespace": runtime_ws::NAMESPACE,
        "socket_path": SOCKET_FILE,
        "owner_uid": unsafe { libc::geteuid() },
        "socket_device": 0,
        "socket_inode": 0,
        "socket_creator_pid": 1,
    });
    std::fs::write(dir.join(POSTBIND_FILE), foreign.to_string()).expect("foreign postbind");

    published.retract().expect("retraction is a no-op here");
    assert!(dir.join(POSTBIND_FILE).exists());
    assert!(dir.join(SOCKET_FILE).exists());
}

/// The daemon holds the namespace lock shared, so a client still takes it while
/// a second publisher's exclusive request is refused.
#[tokio::test]
#[serial_test::serial]
async fn the_held_lock_admits_a_client_and_refuses_a_publisher() {
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    let dir = app_dir(&namespace);
    let _published = publish().expect("the namespace is free");
    let client = open_client_lock(&dir);
    assert!(lock_shared(&client), "a client must be able to lock");
    assert!(!lock_exclusive(&client), "no second publisher may lock");
    unlock(&client);
}

/// A marker written by this process is live; one recorded against another boot
/// or another start time is retained state.
#[test]
fn process_identity_distinguishes_live_from_retained() {
    let pid = std::process::id();
    let live = MarkerProbe {
        schema: SCHEMA,
        pid,
        process_start_identity: process_start_identity(pid)
            .expect("this process has a start identity"),
        prebind_instance_id: "11111111-2222-3333-4444-555555555555".to_string(),
    };
    assert!(process_is_live(&live));
    assert_eq!(
        live.process_start_identity,
        format!(
            "linux:v1:{}:{}",
            boot_id().expect("boot id"),
            process_start_ticks(pid).expect("ticks")
        )
    );

    let recycled = MarkerProbe {
        process_start_identity: format!("linux:v1:{}:0", boot_id().expect("boot id")),
        ..clone_probe(&live)
    };
    assert!(
        !process_is_live(&recycled),
        "a different start time is dead"
    );

    let rebooted = MarkerProbe {
        process_start_identity: "linux:v1:00000000-0000-0000-0000-000000000000:1".to_string(),
        ..clone_probe(&live)
    };
    assert!(!process_is_live(&rebooted), "another boot is dead");
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
    let Some(namespace) = namespace_or_skip() else {
        return;
    };
    assert!(trusted_chain(&app_dir(&namespace)));

    let widened = namespace.base.path().join("widened");
    std::fs::create_dir(&widened).expect("widened");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&widened, std::fs::Permissions::from_mode(0o770)).expect("widen");
    assert!(!trusted_chain(&widened));
    assert_eq!(
        open_trusted_app_dir(&widened)
            .err()
            .map(|error| error.code()),
        Some("app_dir_untrusted"),
        "a group-writable directory is never adopted"
    );
}

/// How a client opens the lock file: a second descriptor on the same inode.
fn open_client_lock(dir: &Path) -> File {
    let name = CString::new(LOCK_FILE).expect("constant");
    let parent = open_dir(dir).expect("app dir fd");
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    assert!(fd >= 0, "the client opens the lock file directly");
    unsafe { File::from_raw_fd(fd) }
}

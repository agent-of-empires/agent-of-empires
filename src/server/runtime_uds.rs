//! Daemon-owned publisher and listener for the local runtime read.
//!
//! The client admits a local read by walking to the app directory it resolves
//! for itself, taking a shared lock on `lifetime.lock`, and comparing the two
//! marker files against the socket, the directory inode and the connecting
//! process. This module is the producer for exactly that contract:
//!
//! * `lifetime.lock` is created 0600 and held `LOCK_SH` for as long as this
//!   daemon is live, so a second daemon cannot take the exclusive lock and
//!   replace a live marker pair.
//! * `runtime.prebind.json` is published before the socket exists and
//!   `runtime.postbind.json` after it is bound, each written to a temporary
//!   name derived from the prebind instance id and then renamed, so a reader
//!   never observes a half-written marker.
//! * `runtime.sock` is a real AF_UNIX listener created 0600 and owned by this
//!   process, and the postbind marker records its real device and inode.
//!
//! Every entry names this process's pid plus a start-time identity, so a
//! retained crash artifact is recognizable as dead rather than merely old.
//! Shutdown removes only what this daemon published.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::runtime_ws;
use super::AppState;

pub(crate) const LOCK_FILE: &str = "lifetime.lock";
pub(crate) const PREBIND_FILE: &str = "runtime.prebind.json";
pub(crate) const POSTBIND_FILE: &str = "runtime.postbind.json";
pub(crate) const SOCKET_FILE: &str = "runtime.sock";

/// Marker schema version. The client refuses anything else.
const SCHEMA: u8 = 1;
/// A stalled reader must not hold a connection slot open indefinitely.
const CONNECTION_BUDGET: Duration = Duration::from_secs(15);
/// Backoff after an accept error, so a failing accept cannot spin the loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// The client's frame ceiling, applied here so a snapshot the client would
/// reject is never produced in the first place.
const FRAME_LIMIT: usize = crate::cli::runtime_read::APPLICATION_LIMIT;

/// Why a daemon could not publish. Each code is a distinct operator-visible
/// condition, and every one of them leaves a foreign publication untouched.
#[derive(Debug)]
pub(crate) struct PublishError {
    code: &'static str,
    detail: String,
}

impl PublishError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PublishError {}

/// The identity a live daemon publishes: the same three values its Hello
/// carries, so a client can prove the peer it reached is the publisher.
#[derive(Debug, Clone)]
struct Published {
    prebind_instance_id: String,
}

#[derive(Serialize)]
struct PrebindMarker {
    schema: u8,
    pid: u32,
    process_start_identity: String,
    prebind_instance_id: String,
    namespace: &'static str,
}

#[derive(Serialize)]
struct PostbindMarker {
    schema: u8,
    pid: u32,
    process_start_identity: String,
    prebind_instance_id: String,
    runtime_instance_id: String,
    runtime_epoch: String,
    namespace: &'static str,
    socket_path: &'static str,
    owner_uid: u32,
    socket_device: u64,
    socket_inode: u64,
    #[cfg(target_os = "linux")]
    socket_creator_pid: u32,
}

/// The only fields any decision about a retained artifact is made from.
#[derive(Deserialize)]
struct MarkerProbe {
    schema: u8,
    pid: u32,
    process_start_identity: String,
    prebind_instance_id: String,
}

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

/// A live publication: the listener, the namespace directory, the held shared
/// lock, and the identity and socket the markers record. Dropping it retracts
/// exactly this daemon's artifacts.
pub(crate) struct PublishedRuntime {
    listener: UnixListener,
    dir: OwnedFd,
    lock: File,
    published: Published,
    socket: SocketIdentity,
    retracted: bool,
}

impl Drop for PublishedRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.retract() {
            tracing::warn!(target: "runtime.uds", %error, "local runtime read artifacts not retracted");
        }
    }
}

impl PublishedRuntime {
    /// Remove this daemon's artifacts and release the lock. A file is removed
    /// only when the publication recorded inside it is ours, so a namespace
    /// that changed hands while this daemon was exiting is left alone.
    fn retract(&mut self) -> Result<(), PublishError> {
        if self.retracted {
            return Ok(());
        }
        self.retracted = true;
        if !lock(&self.lock, libc::LOCK_EX | libc::LOCK_NB) {
            // Another process holds the namespace exclusively, so it owns the
            // artifacts now; retracting would delete a live publication.
            return Err(PublishError::new(
                "namespace_locked",
                "the namespace lock is held exclusively elsewhere",
            ));
        }
        let result = self.retract_locked();
        unlock(&self.lock);
        result
    }

    fn retract_locked(&mut self) -> Result<(), PublishError> {
        let dir = self.dir.as_raw_fd();
        let owns_postbind = self.owns(POSTBIND_FILE)?;
        if owns_postbind {
            if let Some(stat) = entry_stat(dir, SOCKET_FILE)? {
                if stat.st_dev == self.socket.device && stat.st_ino == self.socket.inode {
                    unlink_entry(dir, SOCKET_FILE)?;
                }
            }
        }
        if self.owns(PREBIND_FILE)? {
            unlink_entry(dir, PREBIND_FILE)?;
        }
        if owns_postbind {
            unlink_entry(dir, POSTBIND_FILE)?;
        }
        sync_dir(dir);
        Ok(())
    }

    fn owns(&self, name: &str) -> Result<bool, PublishError> {
        Ok(read_probe(self.dir.as_raw_fd(), name)?
            .is_some_and(|probe| probe.prebind_instance_id == self.published.prebind_instance_id))
    }
}

/// Publish the runtime read into the app directory the client resolves.
///
/// Returns the live publication on success. A daemon that cannot own the
/// namespace does not offer a local read, and says why.
pub(crate) fn publish() -> Result<PublishedRuntime, PublishError> {
    if !cfg!(target_os = "linux") {
        // The client's admission re-derives the boot id and the process start
        // time from /proc, so no marker written elsewhere could be verified.
        return Err(PublishError::new(
            "unsupported_platform",
            "the local runtime read admission is only defined on Linux",
        ));
    }
    let app_dir = crate::session::get_app_dir()
        .map_err(|error| PublishError::new("app_dir_unavailable", error.to_string()))?;
    let dir = open_trusted_app_dir(&app_dir)?;
    let lock = open_lock(dir.as_raw_fd())?;
    if !lock_exclusive(&lock) {
        return Err(PublishError::new(
            "namespace_busy",
            "another live daemon holds the namespace",
        ));
    }
    // From here the namespace is ours: an error retracts what this call wrote.
    publish_locked(dir, lock)
}

fn publish_locked(dir: OwnedFd, lock: File) -> Result<PublishedRuntime, PublishError> {
    let raw = dir.as_raw_fd();
    reap_retained_state(raw)?;
    let identity = runtime_ws::identity();
    let published = Published {
        prebind_instance_id: identity.prebind_instance_id.clone(),
    };
    let process = std::process::id();
    let Some(start) = process_start_identity(process) else {
        return Err(PublishError::new(
            "process_identity",
            "the process start time is unreadable",
        ));
    };
    let owner_uid = unsafe { libc::geteuid() };
    let markers = write_markers(raw, &lock, &published, process, &start, owner_uid, identity);
    match markers {
        Ok(markers) => Ok(PublishedRuntime {
            dir,
            lock,
            published,
            socket: markers.socket,
            listener: markers.listener,
            retracted: false,
        }),
        Err(error) => {
            for name in [
                POSTBIND_FILE,
                PREBIND_FILE,
                SOCKET_FILE,
                &format!("{POSTBIND_FILE}.tmp.{}", published.prebind_instance_id),
                &format!("{PREBIND_FILE}.tmp.{}", published.prebind_instance_id),
            ] {
                let _ = unlink_entry(raw, name);
            }
            Err(error)
        }
    }
}

/// The socket, its recorded identity, and the downgrade from the exclusive
/// publication lock to the shared one held while the read is served.
struct PublishedMarkers {
    listener: UnixListener,
    socket: SocketIdentity,
}

#[allow(clippy::too_many_arguments)]
fn write_markers(
    dir: RawFd,
    lock: &File,
    published: &Published,
    process: u32,
    start: &str,
    owner_uid: u32,
    identity: &runtime_ws::RuntimeIdentity,
) -> Result<PublishedMarkers, PublishError> {
    write_marker(
        dir,
        PREBIND_FILE,
        &published.prebind_instance_id,
        &serde_json::to_vec(&PrebindMarker {
            schema: SCHEMA,
            pid: process,
            process_start_identity: start.to_string(),
            prebind_instance_id: published.prebind_instance_id.clone(),
            namespace: runtime_ws::NAMESPACE,
        })
        .map_err(|error| PublishError::new("marker_encode", error.to_string()))?,
    )?;

    let socket_path = anchored_child_path(dir, SOCKET_FILE)?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| PublishError::new("socket_bind", error.to_string()))?;
    chmod_entry(dir, SOCKET_FILE, 0o600)?;
    let stat = entry_stat(dir, SOCKET_FILE)?
        .ok_or_else(|| PublishError::new("socket_missing", "the bound socket vanished"))?;
    let socket = SocketIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    };

    let postbind = PostbindMarker {
        schema: SCHEMA,
        pid: process,
        process_start_identity: start.to_string(),
        prebind_instance_id: published.prebind_instance_id.clone(),
        runtime_instance_id: identity.runtime_instance_id.clone(),
        runtime_epoch: identity.runtime_epoch.clone(),
        namespace: runtime_ws::NAMESPACE,
        socket_path: SOCKET_FILE,
        owner_uid,
        socket_device: socket.device,
        socket_inode: socket.inode,
        #[cfg(target_os = "linux")]
        socket_creator_pid: process,
    };
    write_marker(
        dir,
        POSTBIND_FILE,
        &published.prebind_instance_id,
        &serde_json::to_vec(&postbind)
            .map_err(|error| PublishError::new("marker_encode", error.to_string()))?,
    )?;
    sync_dir(dir);

    // Hold the namespace for as long as this daemon serves it. The shared lock
    // still refuses a second publisher, which is what keeps a live marker pair
    // from being replaced under a running client.
    unlock(lock);
    if !lock_shared(lock) {
        return Err(PublishError::new(
            "namespace_locked",
            "the namespace was taken while the markers were published",
        ));
    }
    Ok(PublishedMarkers { listener, socket })
}

/// Serve the local read until the daemon shuts down, then retract.
pub(crate) async fn serve(state: Arc<AppState>, mut published: PublishedRuntime) {
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            accepted = published.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    crate::task_util::spawn_supervised(
                        "runtime.uds.connection",
                        crate::task_util::PanicPolicy::Log,
                        connection(state, stream),
                    );
                }
                Err(error) => {
                    tracing::warn!(target: "runtime.uds", %error, "local runtime read accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
        }
    }
    if let Err(error) = published.retract() {
        tracing::warn!(target: "runtime.uds", %error, "local runtime read artifacts not retracted");
    }
}

async fn connection(state: Arc<AppState>, stream: UnixStream) {
    // The socket is 0600 inside a directory only this uid can write, so a peer
    // from another uid is a contract violation rather than a slow reader.
    if peer_uid(&stream) != Some(unsafe { libc::geteuid() }) {
        tracing::warn!(target: "runtime.uds", "refused a local runtime read from another uid");
        return;
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(FRAME_LIMIT))
        .max_frame_size(Some(FRAME_LIMIT));
    let upgraded = tokio::time::timeout(
        CONNECTION_BUDGET,
        tokio_tungstenite::accept_async_with_config(stream, Some(config)),
    )
    .await;
    let socket = match upgraded {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            tracing::debug!(target: "runtime.uds", %error, "local runtime read upgrade refused");
            return;
        }
        Err(_) => {
            tracing::warn!(target: "runtime.uds", "local runtime read handshake timed out");
            return;
        }
    };
    if tokio::time::timeout(
        CONNECTION_BUDGET,
        runtime_ws::serve_runtime_read_uds(socket, state),
    )
    .await
    .is_err()
    {
        tracing::warn!(target: "runtime.uds", "local runtime read exceeded its budget");
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 || length != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return None;
    }
    Some(unsafe { credentials.assume_init() }.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_: &UnixStream) -> Option<u32> {
    None
}

/// Drop every artifact a previous daemon left behind, and refuse to publish
/// while something in the namespace is still owned by a live process.
fn reap_retained_state(dir: RawFd) -> Result<(), PublishError> {
    let retained = retained_names(dir)?;
    // A marker a live process wrote means that process is publishing right
    // now, so nothing here is touched.
    for name in &retained {
        // The socket carries no identity of its own: a live daemon always has a
        // postbind marker beside it, so an unmarked socket is retained state.
        if name == SOCKET_FILE {
            continue;
        }
        let Some(probe) = read_probe(dir, name)? else {
            continue;
        };
        if probe.schema != SCHEMA {
            return Err(PublishError::new(
                "marker_foreign",
                format!("{name} is not a schema {SCHEMA} marker"),
            ));
        }
        if process_is_live(&probe) {
            return Err(PublishError::new(
                "namespace_busy",
                format!("{name} belongs to process {}", probe.pid),
            ));
        }
    }
    for name in &retained {
        unlink_entry(dir, name)?;
    }
    Ok(())
}

/// Every runtime artifact in the directory, temporary names included.
fn retained_names(dir: RawFd) -> Result<Vec<String>, PublishError> {
    let mut found = Vec::new();
    for entry in directory_entries(dir)? {
        let is_runtime = [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE]
            .iter()
            .any(|name| entry == *name)
            || entry.starts_with(&format!("{PREBIND_FILE}.tmp."))
            || entry.starts_with(&format!("{POSTBIND_FILE}.tmp."));
        if is_runtime {
            found.push(entry);
        }
    }
    Ok(found)
}

fn directory_entries(dir: RawFd) -> Result<Vec<String>, PublishError> {
    let duplicate = unsafe { libc::dup(dir) };
    if duplicate < 0 {
        return Err(PublishError::new(
            "namespace_scan",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let scan = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let entries = unsafe { libc::fdopendir(scan.into_raw_fd()) };
    if entries.is_null() {
        return Err(PublishError::new(
            "namespace_scan",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(entries) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        names.push(name.to_string_lossy().into_owned());
    }
    unsafe { libc::closedir(entries) };
    Ok(names)
}

// ---------------------------------------------------------------------------
// Namespace, lock and marker files
// ---------------------------------------------------------------------------

fn open_trusted_app_dir(path: &Path) -> Result<OwnedFd, PublishError> {
    if !path.is_absolute() || !trusted_chain(path) {
        return Err(PublishError::new(
            "app_dir_untrusted",
            format!("{} is not an owned private directory chain", path.display()),
        ));
    }
    let dir = open_dir(path)?;
    // Tighten only what the client's admission rejects; never widen.
    if let Some(stat) = file_stat(dir.as_raw_fd())? {
        if stat.st_mode & 0o022 != 0
            && unsafe { libc::fchmod(dir.as_raw_fd(), stat.st_mode & !0o022) } != 0
        {
            return Err(PublishError::new(
                "app_dir_untrusted",
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(dir)
}

/// The same ancestor rule the client's trusted walk applies: every component
/// must be searchable and owned by root or this uid. A non-final component is
/// additionally tolerated when it is writable by group or others only if it is
/// root-owned and sticky, which is what `/tmp` and a test namespace under it
/// offer.
fn trusted_chain(path: &Path) -> bool {
    let euid = unsafe { libc::geteuid() };
    let components: Vec<CString> = path
        .components()
        .skip(1)
        .map(|component| CString::new(component.as_os_str().as_bytes()))
        .collect::<Result<_, _>>()
        .unwrap_or_default();
    if components.is_empty() {
        return false;
    }
    let Ok(root) = open_dir(Path::new("/")) else {
        return false;
    };
    let mut current = root;
    for (index, component) in components.iter().enumerate() {
        let Ok(next) = open_dir_at(current.as_raw_fd(), component) else {
            return false;
        };
        let Ok(Some(stat)) = file_stat(next.as_raw_fd()) else {
            return false;
        };
        let final_component = index + 1 == components.len();
        let owner_ok = if final_component {
            stat.st_uid == euid
        } else {
            stat.st_uid == 0 || stat.st_uid == euid
        };
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_mode & 0o111 == 0
            || !owner_ok
            || !(stat.st_mode & 0o022 == 0 || (!final_component && sticky_root_directory(&stat)))
        {
            return false;
        }
        current = next;
    }
    true
}

/// A root-owned sticky directory: a stranger may create entries there but
/// cannot rename or replace an entry somebody else owns, so the walk through
/// it is no less safe than through a private one.
fn sticky_root_directory(stat: &libc::stat) -> bool {
    stat.st_uid == 0 && stat.st_mode & libc::S_ISVTX != 0
}

fn open_dir(path: &Path) -> Result<OwnedFd, PublishError> {
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        PublishError::new(
            "app_dir_untrusted",
            "the app directory path is not representable",
        )
    })?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd(fd)
}

fn open_dir_at(parent: RawFd, name: &CString) -> std::io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn owned_fd(fd: RawFd) -> Result<OwnedFd, PublishError> {
    if fd < 0 {
        return Err(PublishError::new(
            "io",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the namespace lock, creating it 0600 if absent, and reject a lock file
/// this daemon does not own outright.
fn open_lock(dir: RawFd) -> Result<File, PublishError> {
    let euid = unsafe { libc::geteuid() };
    let existing = entry_stat(dir, LOCK_FILE)?;
    if let Some(stat) = existing {
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1 || stat.st_uid != euid
        {
            return Err(PublishError::new(
                "lock_foreign",
                format!("{LOCK_FILE} is not an owned regular file"),
            ));
        }
    }
    let name = CString::new(LOCK_FILE).expect("constant");
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(PublishError::new(
            "io",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let opened = file_stat(file.as_raw_fd())?
        .ok_or_else(|| PublishError::new("lock_foreign", "the lock file vanished"))?;
    if opened.st_mode & libc::S_IFMT != libc::S_IFREG
        || opened.st_nlink != 1
        || opened.st_uid != euid
        || existing.is_some_and(|stat| (stat.st_dev, stat.st_ino) != (opened.st_dev, opened.st_ino))
    {
        return Err(PublishError::new(
            "lock_foreign",
            format!("{LOCK_FILE} changed while it was opened"),
        ));
    }
    if opened.st_mode & 0o777 != 0o600 && unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(PublishError::new(
            "lock_foreign",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(file)
}

fn lock(file: &File, operation: libc::c_int) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), operation) == 0 }
}

fn lock_exclusive(file: &File) -> bool {
    lock(file, libc::LOCK_EX | libc::LOCK_NB)
}

fn lock_shared(file: &File) -> bool {
    lock(file, libc::LOCK_SH | libc::LOCK_NB)
}

fn unlock(file: &File) {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

/// Write a marker atomically. The temporary name carries the prebind instance
/// id the client compares the temporary content against, so a reader that finds
/// a torn publication can still tell whose it was.
fn write_marker(dir: RawFd, name: &str, suffix: &str, bytes: &[u8]) -> Result<(), PublishError> {
    let temporary = format!("{name}.tmp.{suffix}");
    let temporary_name = CString::new(temporary.clone()).expect("derived name");
    let final_name = CString::new(name).expect("constant");
    let fd = unsafe {
        libc::openat(
            dir,
            temporary_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(PublishError::new(
            "marker_write",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| PublishError::new("marker_write", error.to_string()));
    drop(file);
    if let Err(error) = written {
        let _ = unlink_entry(dir, &temporary);
        return Err(error);
    }
    if unsafe { libc::renameat(dir, temporary_name.as_ptr(), dir, final_name.as_ptr()) } != 0 {
        let error = PublishError::new("marker_write", std::io::Error::last_os_error().to_string());
        let _ = unlink_entry(dir, &temporary);
        return Err(error);
    }
    Ok(())
}

fn read_probe(dir: RawFd, name: &str) -> Result<Option<MarkerProbe>, PublishError> {
    let Some(stat) = entry_stat(dir, name)? else {
        return Ok(None);
    };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1 {
        return Err(PublishError::new(
            "marker_foreign",
            format!("{name} is not a regular file"),
        ));
    }
    let entry = CString::new(name).expect("derived name");
    let fd = unsafe {
        libc::openat(
            dir,
            entry.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(PublishError::new(
            "marker_foreign",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| PublishError::new("marker_foreign", error.to_string()))?;
    Ok(Some(serde_json::from_slice(&bytes).map_err(|error| {
        PublishError::new("marker_foreign", error.to_string())
    })?))
}

fn entry_stat(dir: RawFd, name: &str) -> Result<Option<libc::stat>, PublishError> {
    let entry = CString::new(name).expect("derived name");
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            dir,
            entry.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(Some(unsafe { stat.assume_init() }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(PublishError::new("io", error.to_string()))
    }
}

fn file_stat(fd: RawFd) -> Result<Option<libc::stat>, PublishError> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(PublishError::new(
            "io",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(Some(unsafe { stat.assume_init() }))
}

fn unlink_entry(dir: RawFd, name: &str) -> Result<(), PublishError> {
    let entry = CString::new(name).expect("derived name");
    if unsafe { libc::unlinkat(dir, entry.as_ptr(), 0) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(PublishError::new("unlink", error.to_string()));
        }
    }
    Ok(())
}

fn chmod_entry(dir: RawFd, name: &str, mode: libc::mode_t) -> Result<(), PublishError> {
    let entry = CString::new(name).expect("derived name");
    if unsafe { libc::fchmodat(dir, entry.as_ptr(), mode, 0) } != 0 {
        return Err(PublishError::new(
            "socket_chmod",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}

fn sync_dir(dir: RawFd) {
    let _ = unsafe { libc::fsync(dir) };
}

/// The client's anchored alias for a child of the namespace directory, so the
/// socket is bound to the directory this daemon opened rather than to a path
/// that could be swapped underneath it.
fn anchored_child_path(dir: RawFd, child: &str) -> Result<PathBuf, PublishError> {
    let base = PathBuf::from(format!("/proc/self/fd/{dir}"));
    if !base.exists() {
        return Err(PublishError::new(
            "anchored_alias_unavailable",
            "this platform has no directory fd alias",
        ));
    }
    Ok(base.join(child))
}

// ---------------------------------------------------------------------------
// Process identity
// ---------------------------------------------------------------------------

/// `linux:v1:<boot id>:<start time ticks>`. The client recomputes both halves,
/// so a recycled pid cannot pass as this daemon.
fn process_start_identity(pid: u32) -> Option<String> {
    let boot = boot_id()?;
    Some(format!("linux:v1:{boot}:{}", process_start_ticks(pid)?))
}

fn boot_id() -> Option<String> {
    Some(
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim()
            .to_string(),
    )
}

fn process_start_ticks(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_string)
}

/// Whether the process that wrote a retained marker is still running. A marker
/// from a boot that has ended, or a pid that has been recycled, is dead.
fn process_is_live(probe: &MarkerProbe) -> bool {
    let Some(rest) = probe.process_start_identity.strip_prefix("linux:v1:") else {
        return false;
    };
    let Some((recorded_boot, recorded_start)) = rest.split_once(':') else {
        return false;
    };
    match boot_id() {
        // Without a boot id nothing can be proven dead, so fail closed.
        None => true,
        Some(boot) if boot != recorded_boot => false,
        Some(_) => process_start_ticks(probe.pid).as_deref() == Some(recorded_start),
    }
}

#[cfg(test)]
mod tests;

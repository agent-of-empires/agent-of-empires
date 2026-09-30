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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::runtime_ws;
use super::runtime_ws::{CONNECTION_BUDGET, MESSAGE_LIMIT};
use super::AppState;

pub(crate) const LOCK_FILE: &str = "lifetime.lock";
pub(crate) const PREBIND_FILE: &str = "runtime.prebind.json";
pub(crate) const POSTBIND_FILE: &str = "runtime.postbind.json";
pub(crate) const SOCKET_FILE: &str = "runtime.sock";

/// Marker schema version. The client refuses anything else, and imports this
/// constant rather than spelling the number a second time.
pub(crate) const SCHEMA: u8 = 1;
/// What a create-then-rename marker is called before its rename: the final
/// name, this, and the prebind instance id the write belongs to. The client
/// refuses these names, so both halves have to spell them the one way.
pub(crate) const TEMPORARY_SEPARATOR: &str = ".tmp.";
/// Backoff after an accept error, so a failing accept cannot spin the loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// The client's frame ceiling, applied to what this endpoint reads. tungstenite
/// consults it on the receive path only, so it bounds nothing written here; the
/// frames this endpoint's reader would send are capped by
/// [`MESSAGE_LIMIT`], which is checked before a frame leaves.
const FRAME_LIMIT: usize = MESSAGE_LIMIT;

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
        // The artifacts are unlinked under the shared lock this daemon already
        // holds, not under a fresh exclusive one: every client holds `LOCK_SH`
        // for its whole exchange, so asking for `LOCK_EX` would fail for the
        // entire life of any read and strand the three artifacts forever. The
        // shared lock already excludes a competing publisher, and `retract_locked`
        // proves ownership per file by comparing the recorded prebind instance
        // id and re-checking the socket's device and inode.
        let result = self.retract_locked();
        unlock(&self.lock);
        if result.is_ok() {
            self.retracted = true;
        }
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
                &format!(
                    "{POSTBIND_FILE}{TEMPORARY_SEPARATOR}{}",
                    published.prebind_instance_id
                ),
                &format!(
                    "{PREBIND_FILE}{TEMPORARY_SEPARATOR}{}",
                    published.prebind_instance_id
                ),
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
    // One budget for the whole connection, handshake and read alike: two
    // separate windows would let a peer spend twice the client's own budget by
    // stalling in the handshake.
    let deadline = tokio::time::Instant::now() + CONNECTION_BUDGET;
    let config = WebSocketConfig::default()
        .max_message_size(Some(runtime_ws::MESSAGE_LIMIT))
        .max_frame_size(Some(FRAME_LIMIT));
    let upgraded = tokio::time::timeout_at(
        deadline,
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
    if tokio::time::timeout_at(deadline, runtime_ws::serve_runtime_read_uds(socket, state))
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
    // A retained temporary name is a half-written marker. One whose writer is
    // still running is publication in progress and is left alone. Anything
    // else, including the body a crash between the exclusive create and the
    // rename leaves behind, which does not parse, is unlinked before any final
    // name is judged, so a torn temporary cannot refuse publication forever.
    for name in retained.iter().filter(|name| is_temporary_name(name)) {
        if temporary_is_live(dir, name)? {
            return Err(PublishError::new(
                "namespace_busy",
                format!("{name} belongs to a process still publishing"),
            ));
        }
        unlink_entry(dir, name)?;
    }
    // Only a marker proven gone is reaped. A live writer is publishing right
    // now and a marker this half cannot place proves nothing at all, so both
    // leave everything in place, and the refusal names whichever of the two
    // reasons applies.
    for name in retained.iter().filter(|name| !is_temporary_name(name)) {
        // The socket carries no identity of its own: a live daemon always has a
        // postbind marker beside it, so an unmarked socket is retained state.
        if name == SOCKET_FILE {
            continue;
        }
        let Some(probe) = read_probe(dir, name)? else {
            continue;
        };
        if process_liveness(&probe) != ProcessLiveness::Dead {
            return Err(if probe.schema == SCHEMA {
                PublishError::new(
                    "namespace_busy",
                    format!("{name} belongs to process {}", probe.pid),
                )
            } else {
                PublishError::new(
                    "marker_foreign",
                    format!("{name} is not a schema {SCHEMA} marker"),
                )
            });
        }
    }
    for name in &retained {
        unlink_entry(dir, name)?;
    }
    Ok(())
}

/// Whether `name` is a create-then-rename marker of `file` that has not been
/// renamed yet. One definition, because the client refuses these names and a
/// second spelling would be a name the client does not recognise.
pub(crate) fn is_temporary_of(name: &str, file: &str) -> bool {
    name.strip_prefix(file)
        .is_some_and(|rest| rest.starts_with(TEMPORARY_SEPARATOR))
}

/// A create-then-rename marker's temporary name.
fn is_temporary_name(name: &str) -> bool {
    is_temporary_of(name, PREBIND_FILE) || is_temporary_of(name, POSTBIND_FILE)
}

/// Whether a retained temporary marker still has a live writer. A body that
/// does not parse is not in flight: a writer that is still running has not
/// finished its write, and only a finished write can be refused. A body that
/// cannot even be *read* proves nothing about a writer, so it is not reaped
/// on a guess: publication is refused and the name is left in place, exactly
/// as the client half refuses to leave `marker_identity` on an unreadable
/// `/proc`.
fn temporary_is_live(dir: RawFd, name: &str) -> Result<bool, PublishError> {
    let Some(stat) = entry_stat(dir, name)? else {
        return Ok(false);
    };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1 {
        return Ok(false);
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
        // A name that vanished between the scan and the open is definitively
        // gone; any other errno is a question this process cannot answer.
        let unprovable = std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT);
        return if unprovable {
            Err(PublishError::new(
                "namespace_busy",
                format!("{name} could not be read, so its writer cannot be ruled out"),
            ))
        } else {
            Ok(false)
        };
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return Err(PublishError::new(
            "namespace_busy",
            format!("{name} could not be read: {error}"),
        ));
    }
    Ok(serde_json::from_slice::<MarkerProbe>(&bytes)
        .is_ok_and(|probe| process_liveness(&probe) == ProcessLiveness::Live))
}

/// Every runtime artifact in the directory, temporary names included.
fn retained_names(dir: RawFd) -> Result<Vec<String>, PublishError> {
    let mut found = Vec::new();
    for entry in directory_entries(dir)? {
        let is_runtime = [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE]
            .iter()
            .any(|name| entry == *name)
            || is_temporary_name(&entry);
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
    let raw = scan.into_raw_fd();
    let entries = unsafe { libc::fdopendir(raw) };
    if entries.is_null() {
        unsafe { libc::close(raw) };
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

/// The namespace directory the client admits, as the descriptor that walk ends
/// on.
///
/// The client is the enforcing boundary for which directories a local read may
/// live in, so the producer runs the client's own walk
/// (`cli::runtime_read::uds::open_trusted_directory`) rather than a copy of
/// it. A prefix symlink, a home reached through one, is followed and the
/// directory it resolves to is verified by descriptor, a symlinked app
/// directory is a refusal, and every resolved component must satisfy the
/// ownership, group/other-write, sticky-root and POSIX-ACL rules.
///
/// The returned descriptor is the walk's final one, so nothing downstream
/// re-resolves the app directory by name: the lock, the markers, the socket
/// and the retraction all address this inode.
fn open_trusted_app_dir(path: &Path) -> Result<OwnedFd, PublishError> {
    client_trusted_directory(path).map_err(|_| {
        PublishError::new(
            "app_dir_untrusted",
            format!(
                "{} is not a directory chain the client admits",
                path.display()
            ),
        )
    })
}

/// The client's walk, error collapsed: both of its refusals, an unreadable
/// chain and one that does not exist, mean the same thing to a producer,
/// which cannot offer a read the client would refuse.
#[cfg(target_os = "linux")]
fn client_trusted_directory(path: &Path) -> Result<OwnedFd, ()> {
    crate::cli::runtime_read::uds::open_trusted_directory(path, unsafe { libc::geteuid() })
        .map_err(|_| ())
}

#[cfg(not(target_os = "linux"))]
fn client_trusted_directory(_: &Path) -> Result<OwnedFd, ()> {
    Err(())
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
    // Unconditional, and on this descriptor rather than by name: `0o600` is a
    // creation mode, so a umask that masks an owner bit lands the marker at
    // less than the client admits, and the client's `validate_regular_file`
    // requires exactly `0o600`. The conditional form `open_lock` uses is
    // available there because it re-stats the inode; this path has no such
    // re-stat, and a name-based chmod could be raced between the chmod and
    // the rename below.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        let error = PublishError::new("marker_write", std::io::Error::last_os_error().to_string());
        drop(file);
        let _ = unlink_entry(dir, &temporary);
        return Err(error);
    }
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
    let ProcessStart::Ticks(ticks) = process_start_ticks(pid) else {
        return None;
    };
    Some(format!("linux:v1:{boot}:{ticks}"))
}

fn boot_id() -> Option<String> {
    Some(
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim()
            .to_string(),
    )
}

/// A three-way answer, mirroring the client half (`cli::runtime_read::uds`):
/// a start time on a successful read, absent only on a definite
/// `NotFound`, and a read that failed for any other reason stays *unprovable*
///: a transient `EMFILE`/`ENFILE`/`EACCES`, or a `/proc` this namespace
/// cannot see, is not evidence that the process is gone.
enum ProcessStart {
    Ticks(String),
    Absent,
    Unprovable,
}

fn process_start_ticks(pid: u32) -> ProcessStart {
    #[cfg(test)]
    if PROC_READ_UNPROVABLE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return ProcessStart::Unprovable;
    }
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ProcessStart::Absent,
        Err(_) => return ProcessStart::Unprovable,
    };
    // A body that parses as neither a stat line nor nothing is unreadable as
    // evidence, the same as a read that failed.
    let Some(close) = stat.rfind(')') else {
        return ProcessStart::Unprovable;
    };
    match stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        Some(ticks) => ProcessStart::Ticks(ticks.to_string()),
        None => ProcessStart::Unprovable,
    }
}

/// Test-only: makes the next `/proc/<pid>/stat` reads report `Unprovable`, the
/// way an `EACCES` or a pid-namespace mismatch does. Manufacturing an
/// unreadable `/proc` entry for a real pid needs privileges the suite does not
/// have; the behaviour under test is the branch, not the errno.
#[cfg(test)]
static PROC_READ_UNPROVABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn fail_next_proc_read() {
    PROC_READ_UNPROVABLE.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// A three-way answer about a retained marker's writer. `Live` and `Dead` are
/// proven; `Unprovable` is everything else, and the only value that may be
/// reaped is `Dead`. An identity string this platform cannot parse is
/// unprovable rather than an absence. That rule is stated once, by
/// [`crate::cli::runtime_read::uds::valid_process_identity`], which the client
/// half turns into `marker_identity` and this half turns into this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLiveness {
    Live,
    Dead,
    Unprovable,
}

/// Whether the process that wrote a retained marker is still running. A marker
/// from a boot that has ended, or a pid that has been recycled, is dead.
fn process_liveness(probe: &MarkerProbe) -> ProcessLiveness {
    if !crate::cli::runtime_read::uds::valid_process_identity(&probe.process_start_identity) {
        return ProcessLiveness::Unprovable;
    }
    // The predicate has already established both of these, so a mismatch here
    // is a predicate that stopped describing the marker, and unprovable is the
    // answer that cannot reap a live daemon's state.
    let Some(rest) = probe.process_start_identity.strip_prefix("linux:v1:") else {
        return ProcessLiveness::Unprovable;
    };
    let Some((recorded_boot, recorded_start)) = rest.split_once(':') else {
        return ProcessLiveness::Unprovable;
    };
    let Some(boot) = boot_id() else {
        // Without a boot id nothing can be proven dead, so fail closed.
        return ProcessLiveness::Unprovable;
    };
    if boot != recorded_boot {
        return ProcessLiveness::Dead;
    }
    match process_start_ticks(probe.pid) {
        ProcessStart::Ticks(start) if start == recorded_start => ProcessLiveness::Live,
        ProcessStart::Ticks(_) => ProcessLiveness::Dead,
        // Only a proven-absent process is reaped.
        ProcessStart::Absent => ProcessLiveness::Dead,
        // An unreadable `/proc` proves nothing, so the retained state of a
        // possibly-running daemon is left alone and publication is refused.
        // Reaping here would unlink a live daemon's markers and publish over
        // its socket, and clients would fall back silently while that daemon
        // kept serving the listener.
        ProcessStart::Unprovable => ProcessLiveness::Unprovable,
    }
}

#[cfg(test)]
mod tests;

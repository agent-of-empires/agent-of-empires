//! Daemon-owned publisher and listener for the local runtime read.
//!
//! `publisher.lock` stays exclusive from claim through retraction, including
//! the wait for retained readers and the lifetime-lock downgrade.
//! `lifetime.lock` is exclusive while publishing, then shared while serving.
//! Markers are atomically replaced; rollback and shutdown remove only matching
//! publication identities and the socket inode created by this publisher.

use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::runtime_ws;
use super::runtime_ws::{CONNECTION_BUDGET, MESSAGE_LIMIT};
use super::AppState;
#[cfg(test)]
use crate::process::runtime_io::fail_next_proc_read;
use crate::process::runtime_io::{
    self as os, boot_id, process_start_identity, process_start_ticks, ProcessStart,
};

pub(crate) const LOCK_FILE: &str = "lifetime.lock";
pub(crate) const PUBLISHER_LOCK_FILE: &str = "publisher.lock";
pub(crate) const PREBIND_FILE: &str = "runtime.prebind.json";
pub(crate) const POSTBIND_FILE: &str = "runtime.postbind.json";
pub(crate) const SOCKET_FILE: &str = "runtime.sock";

/// Marker schema version. The client refuses anything else, and imports this
/// constant rather than spelling the number a second time.
pub(crate) const SCHEMA: u8 = 1;
/// Temporary marker suffix, followed by the prebind instance id.
pub(crate) const TEMPORARY_SEPARATOR: &str = ".tmp.";
/// Backoff after an accept error, so a failing accept cannot spin the loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
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

/// A live publication; dropping it retracts its matching artifacts.
pub(crate) struct PublishedRuntime {
    listener: UnixListener,
    dir: OwnedFd,
    lock: File,
    owner: Option<File>,
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
    /// Retract matching markers and the socket inode created by this publication.
    fn retract(&mut self) -> Result<(), PublishError> {
        if self.retracted {
            return Ok(());
        }
        // The separate publisher lock excludes writers throughout retraction.
        let result = self.retract_locked();
        unlock(&self.lock);
        if result.is_ok() {
            self.retracted = true;
            self.owner.take();
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

#[cfg(any(test, debug_assertions))]
pub(crate) fn try_publish() -> Result<PublishedRuntime, PublishError> {
    let (dir, owner) = open_namespace()?;
    if !lock_exclusive(&owner)? {
        return Err(PublishError::new(
            "namespace_busy",
            "another publisher owns the namespace",
        ));
    }
    let lock = open_lock(dir.as_raw_fd(), LOCK_FILE)?;
    if !lock_exclusive(&lock)? {
        return Err(PublishError::new(
            "reader_busy",
            "a runtime reader still holds the lifetime lock",
        ));
    }
    publish_locked(dir, lock, owner)
}

fn open_namespace() -> Result<(OwnedFd, File), PublishError> {
    let app_dir = crate::session::get_app_dir()
        .map_err(|error| PublishError::new("app_dir_unavailable", error.to_string()))?;
    let dir = open_trusted_app_dir(&app_dir)?;
    let owner = open_lock(dir.as_raw_fd(), PUBLISHER_LOCK_FILE)?;
    Ok((dir, owner))
}

pub(crate) async fn publish_when_available(
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Option<PublishedRuntime>, PublishError> {
    let (dir, owner) = open_namespace()?;
    loop {
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        if lock_exclusive(&owner)? {
            break;
        }
        // Shared absence observers are not exclusive publishers.
        match fs2::FileExt::try_lock_shared(&owner) {
            Ok(()) => fs2::FileExt::unlock(&owner)
                .map_err(|error| PublishError::new("lock_io", error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(PublishError::new(
                    "namespace_busy",
                    "another publisher owns the namespace",
                ));
            }
            Err(error) => return Err(PublishError::new("lock_io", error.to_string())),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(None),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    let lock = open_lock(dir.as_raw_fd(), LOCK_FILE)?;
    loop {
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        if lock_exclusive(&lock)? {
            return publish_locked(dir, lock, owner).map(Some);
        }
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(None),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
}

fn publish_locked(dir: OwnedFd, lock: File, owner: File) -> Result<PublishedRuntime, PublishError> {
    let raw = dir.as_raw_fd();
    reap_retained_state(raw)?;
    let identity = runtime_ws::identity();
    let published = Published {
        prebind_instance_id: identity.prebind_instance_id.clone(),
    };
    let mut created_socket = None;
    match write_markers(raw, &lock, &published, identity, &mut created_socket) {
        Ok(markers) => Ok(PublishedRuntime {
            dir,
            lock,
            owner: Some(owner),
            published,
            socket: markers.socket,
            listener: markers.listener,
            retracted: false,
        }),
        Err(error) => {
            if let Some(socket) = created_socket {
                if entry_stat(raw, SOCKET_FILE)
                    .ok()
                    .flatten()
                    .is_some_and(|stat| stat.st_dev == socket.device && stat.st_ino == socket.inode)
                {
                    let _ = unlink_entry(raw, SOCKET_FILE);
                }
            }
            for name in [POSTBIND_FILE, PREBIND_FILE] {
                if read_probe(raw, name)
                    .ok()
                    .flatten()
                    .is_some_and(|probe| probe.prebind_instance_id == published.prebind_instance_id)
                {
                    let _ = unlink_entry(raw, name);
                }
            }
            sync_dir(raw);
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

fn write_markers(
    dir: RawFd,
    lock: &File,
    published: &Published,
    identity: &runtime_ws::RuntimeIdentity,
    created_socket: &mut Option<SocketIdentity>,
) -> Result<PublishedMarkers, PublishError> {
    let process = std::process::id();
    let start = process_start_identity(process).ok_or_else(|| {
        PublishError::new("process_identity", "the process start time is unreadable")
    })?;
    let owner_uid = crate::process::effective_uid();
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
    let stat = entry_stat(dir, SOCKET_FILE)?
        .ok_or_else(|| PublishError::new("socket_missing", "the bound socket vanished"))?;
    let socket = SocketIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    };
    *created_socket = Some(socket);
    chmod_entry(dir, SOCKET_FILE, 0o600)?;

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
        socket_device: socket.device as _,
        socket_inode: socket.inode as _,
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

    // The publisher lock remains exclusive across the lifetime-lock handoff.
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
    if peer_uid(&stream) != Some(crate::process::effective_uid()) {
        tracing::warn!(target: "runtime.uds", "refused a local runtime read from another uid");
        return;
    }
    // Handshake and application exchange share one deadline.
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

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    os::peer_credentials(stream.as_raw_fd())
        .ok()
        .map(|peer| peer.uid)
}

/// Drop every artifact a previous daemon left behind, and refuse to publish
/// while something in the namespace is still owned by a live process.
fn reap_retained_state(dir: RawFd) -> Result<(), PublishError> {
    let retained = retained_names(dir)?;
    // Reap incomplete temporaries and proven-dead writers, not live or unprovable writers.
    for name in retained.iter().filter(|name| is_temporary_name(name)) {
        if temporary_is_live(dir, name)? {
            return Err(PublishError::new(
                "namespace_busy",
                format!("{name} belongs to a process still publishing"),
            ));
        }
        unlink_entry(dir, name)?;
    }
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

/// Shared recognition of create-then-rename marker names.
pub(crate) fn is_temporary_of(name: &str, file: &str) -> bool {
    name.strip_prefix(file)
        .is_some_and(|rest| rest.starts_with(TEMPORARY_SEPARATOR))
}

/// A create-then-rename marker's temporary name.
fn is_temporary_name(name: &str) -> bool {
    is_temporary_of(name, PREBIND_FILE) || is_temporary_of(name, POSTBIND_FILE)
}

/// Parsed live or unprovable writers remain; incomplete or proven-dead writes may be reaped.
fn temporary_is_live(dir: RawFd, name: &str) -> Result<bool, PublishError> {
    let Some(stat) = entry_stat(dir, name)? else {
        return Ok(false);
    };
    if stat.st_mode & crate::process::runtime_io::KIND_MASK != crate::process::runtime_io::REGULAR
        || stat.st_nlink != 1
    {
        return Ok(false);
    }
    let entry = CString::new(name).expect("derived name");
    let mut file = match os::open_readonly_at(dir, &entry) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(PublishError::new(
                "namespace_busy",
                format!("{name} could not be read: {error}"),
            ))
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return Err(PublishError::new(
            "namespace_busy",
            format!("{name} could not be read: {error}"),
        ));
    }
    Ok(serde_json::from_slice::<MarkerProbe>(&bytes)
        .is_ok_and(|probe| !matches!(process_liveness(&probe), ProcessLiveness::Dead)))
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
    os::read_directory(dir)
        .map_err(|error| PublishError::new("namespace_scan", error.to_string()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| PublishError::new("namespace_scan", error.to_string()))
        })
        .collect()
}

/// Reuse client descriptor admission; downstream operations address the admitted inode.
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

#[cfg(target_os = "linux")]
fn client_trusted_directory(path: &Path) -> Result<OwnedFd, ()> {
    crate::cli::runtime_read::uds::open_trusted_directory(path, crate::process::effective_uid())
        .map_err(|_| ())
}

/// Open the namespace lock, creating it 0600 if absent, and reject a lock file
/// this daemon does not own outright.
fn open_lock(dir: RawFd, entry_name: &str) -> Result<File, PublishError> {
    let euid = crate::process::effective_uid();
    let existing = entry_stat(dir, entry_name)?;
    if let Some(stat) = existing {
        if stat.st_mode & crate::process::runtime_io::KIND_MASK
            != crate::process::runtime_io::REGULAR
            || stat.st_nlink != 1
            || stat.st_uid != euid
        {
            return Err(PublishError::new(
                "lock_foreign",
                format!("{entry_name} is not an owned regular file"),
            ));
        }
    }
    let name = CString::new(entry_name).expect("constant");
    let file =
        os::open_lock_at(dir, &name).map_err(|error| PublishError::new("io", error.to_string()))?;
    let opened = file_stat(file.as_raw_fd())?
        .ok_or_else(|| PublishError::new("lock_foreign", "the lock file vanished"))?;
    if opened.st_mode & crate::process::runtime_io::KIND_MASK != crate::process::runtime_io::REGULAR
        || opened.st_nlink != 1
        || opened.st_uid != euid
        || existing.is_some_and(|stat| (stat.st_dev, stat.st_ino) != (opened.st_dev, opened.st_ino))
    {
        return Err(PublishError::new(
            "lock_foreign",
            format!("{entry_name} changed while it was opened"),
        ));
    }
    if opened.st_mode & 0o777 != 0o600 {
        os::set_file_mode(file.as_raw_fd(), 0o600)
            .map_err(|error| PublishError::new("lock_foreign", error.to_string()))?;
    }
    Ok(file)
}

fn lock_exclusive(file: &File) -> Result<bool, PublishError> {
    match fs2::FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(PublishError::new("lock_io", error.to_string())),
    }
}

fn lock_shared(file: &File) -> bool {
    fs2::FileExt::try_lock_shared(file).is_ok()
}

fn unlock(file: &File) {
    let _ = fs2::FileExt::unlock(file);
}

/// Include the publication UUID in the temporary name before atomic rename.
fn write_marker(dir: RawFd, name: &str, suffix: &str, bytes: &[u8]) -> Result<(), PublishError> {
    os::write_atomic(dir, name, &format!("{name}.tmp.{suffix}"), bytes)
        .map_err(|error| PublishError::new("marker_write", error.to_string()))
}

fn read_probe(dir: RawFd, name: &str) -> Result<Option<MarkerProbe>, PublishError> {
    let Some(stat) = entry_stat(dir, name)? else {
        return Ok(None);
    };
    if stat.st_mode & crate::process::runtime_io::KIND_MASK != crate::process::runtime_io::REGULAR
        || stat.st_nlink != 1
    {
        return Err(PublishError::new(
            "marker_foreign",
            format!("{name} is not a regular file"),
        ));
    }
    let entry = CString::new(name).expect("derived name");
    let mut file = os::open_readonly_at(dir, &entry)
        .map_err(|error| PublishError::new("marker_foreign", error.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| PublishError::new("marker_foreign", error.to_string()))?;
    Ok(Some(serde_json::from_slice(&bytes).map_err(|error| {
        PublishError::new("marker_foreign", error.to_string())
    })?))
}

fn entry_stat(dir: RawFd, name: &str) -> Result<Option<os::Stat>, PublishError> {
    match os::stat_entry(dir, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PublishError::new("io", error.to_string())),
    }
}
fn file_stat(fd: RawFd) -> Result<Option<os::Stat>, PublishError> {
    os::stat_file(fd)
        .map(Some)
        .map_err(|error| PublishError::new("io", error.to_string()))
}

fn unlink_entry(dir: RawFd, name: &str) -> Result<(), PublishError> {
    os::unlink_entry(dir, name).map_err(|error| PublishError::new("unlink", error.to_string()))
}
fn chmod_entry(dir: RawFd, name: &str, mode: u32) -> Result<(), PublishError> {
    os::set_entry_mode(dir, name, mode)
        .map_err(|error| PublishError::new("socket_chmod", error.to_string()))
}
fn sync_dir(dir: RawFd) {
    os::sync_directory(dir);
}
fn anchored_child_path(dir: RawFd, child: &str) -> Result<PathBuf, PublishError> {
    os::anchored_path(dir, child)
        .map_err(|error| PublishError::new("anchored_alias_unavailable", error.to_string()))
}

/// Only a proven-dead writer permits reaping retained artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLiveness {
    #[cfg(target_os = "linux")]
    Live,
    Dead,
    Unprovable,
}

/// Whether the process that wrote a retained marker is still running. A marker
/// from a boot that has ended, or a pid that has been recycled, is dead.
#[cfg(target_os = "linux")]
fn process_liveness(probe: &MarkerProbe) -> ProcessLiveness {
    if !crate::cli::runtime_read::uds::valid_process_identity(&probe.process_start_identity) {
        return ProcessLiveness::Unprovable;
    }
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
        // Unreadable process state cannot authorize replacing a possibly-live publication.
        ProcessStart::Unprovable => ProcessLiveness::Unprovable,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

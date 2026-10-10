use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::net::UnixStream;
use tokio::time::Instant;

use super::dto::{valid_namespace, valid_uuid};
use super::ReadFailure;
use crate::process::runtime_io::{self as os, validate_posix_acl, AclError};
#[cfg(test)]
use crate::process::runtime_io::{
    validate_acl_value, ACL_GROUP, ACL_GROUP_OBJ, ACL_MASK, ACL_OTHER, ACL_USER, ACL_USER_OBJ,
    ACL_VERSION,
};
use crate::server::runtime_uds::{
    is_temporary_of, LOCK_FILE, POSTBIND_FILE, PREBIND_FILE, PUBLISHER_LOCK_FILE, SCHEMA,
    SOCKET_FILE, TEMPORARY_SEPARATOR,
};

const MARKER_LIMIT: u64 = 64 * 1024;
/// Backoff avoids spinning within the bounded publication-establishment budget.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrustedPathError {
    Missing,
    Invalid,
    /// An inadmissible directory with an actionable diagnostic.
    Refused(RefusedComponent),
}

/// Refused path component, descriptor mode and actionable cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefusedComponent {
    path: PathBuf,
    mode: String,
    cause: &'static str,
    remedy: &'static str,
}

impl RefusedComponent {
    fn new(path: &Path, mode: u32, cause: &'static str, remedy: &'static str) -> Self {
        Self {
            path: path.to_path_buf(),
            mode: format!("{mode:04o}"),
            cause,
            remedy,
        }
    }

    /// Identify the refused directory, its mode, cause and safe remedy.
    fn message(&self) -> String {
        format!(
            "refused to read the daemon's runtime state: {} is {} (mode {}).\n\
             Resolve this by: {}\n",
            self.path.display(),
            self.cause,
            self.mode,
            self.remedy
                .replace("{path}", &shell_word(&self.path.to_string_lossy())),
        )
    }
}

/// A path quoted for the shell the remedy is typed into, so a home with a
/// space in it is still one word.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) struct OwnedNamespace {
    pub name: String,
    pub home: PathBuf,
    dir: OwnedFd,
}

impl OwnedNamespace {
    fn anchored_socket_path(&self) -> Result<PathBuf, ReadFailure> {
        anchored_child_path(self.dir.as_raw_fd(), SOCKET_FILE)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrebindMarker {
    schema: u8,
    pid: u32,
    process_start_identity: String,
    prebind_instance_id: String,
    namespace: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostbindMarker {
    schema: u8,
    pid: u32,
    process_start_identity: String,
    prebind_instance_id: String,
    runtime_instance_id: String,
    runtime_epoch: String,
    namespace: String,
    socket_path: String,
    owner_uid: u32,
    socket_device: u64,
    socket_inode: u64,
    socket_creator_pid: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct UdsIdentity {
    pub namespace: String,
    pub prebind_instance_id: String,
    pub runtime_instance_id: String,
    pub runtime_epoch: String,
    pub owner_uid: u32,
}

pub(crate) struct UdsConnection {
    stream: UnixStream,
    identity: UdsIdentity,
    home: PathBuf,
    admission: Admission,
    exchange_deadline: Instant,
}

pub(crate) struct UdsExchange {
    pub stream: tokio_tungstenite::WebSocketStream<UnixStream>,
    pub identity: UdsIdentity,
    pub home: PathBuf,
    pub deadline: Instant,
    pub _admission: Admission,
}

impl UdsConnection {
    pub(crate) async fn upgrade(
        self,
        request: tokio_tungstenite::tungstenite::handshake::client::Request,
    ) -> Result<UdsExchange, ReadFailure> {
        let config = super::websocket_config();
        let (stream, _) = tokio::time::timeout_at(
            self.exchange_deadline,
            tokio_tungstenite::client_async_with_config(request, self.stream, Some(config)),
        )
        .await
        .map_err(|_| ReadFailure::post("publisher_absent"))?
        .map_err(|_| ReadFailure::post("publisher_absent"))?;
        Ok(UdsExchange {
            stream,
            identity: self.identity,
            home: self.home,
            deadline: self.exchange_deadline,
            _admission: self.admission,
        })
    }
}

pub(crate) struct Admission {
    _namespace: OwnedNamespace,
    _lock: File,
}

/// Retry publication identity races within the establishment budget.
/// Untrusted artifacts refuse; absent or provably dead publication permits disk fallback.
pub(crate) fn connect(
    establishment_deadline: Instant,
) -> impl std::future::Future<Output = Result<UdsConnection, ReadFailure>> {
    #[cfg(test)]
    {
        connect_inner(establishment_deadline, |_| {})
    }
    #[cfg(not(test))]
    {
        connect_inner(establishment_deadline)
    }
}

async fn connect_inner(
    establishment_deadline: Instant,
    #[cfg(test)] mut on_retry: impl FnMut(&ReadFailure),
) -> Result<UdsConnection, ReadFailure> {
    loop {
        let namespace = tokio::time::timeout_at(
            establishment_deadline,
            tokio::task::spawn_blocking(existing_app_namespace),
        )
        .await
        .map_err(|_| ReadFailure::pre("establishment_timeout"))?
        .map_err(|_| ReadFailure::post("unavailable"))?
        .map_err(trusted_path_failure)?;
        let attempt = tokio::time::timeout_at(
            establishment_deadline,
            connect_admission(namespace, establishment_deadline),
        )
        .await;
        match attempt {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(error)) if error.code() == "marker_identity" => {
                #[cfg(test)]
                on_retry(&error);
                if wait_for_retry(establishment_deadline).await.is_err() {
                    return Err(ReadFailure::pre("establishment_timeout"));
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(ReadFailure::pre("establishment_timeout")),
        }
    }
}

/// Sleep until the next admission attempt, or report that the budget is spent.
async fn wait_for_retry(establishment_deadline: Instant) -> Result<(), ()> {
    let now = Instant::now();
    if now >= establishment_deadline {
        return Err(());
    }
    tokio::time::sleep(RETRY_INTERVAL.min(establishment_deadline - now)).await;
    Ok(())
}

fn trusted_path_failure(error: TrustedPathError) -> ReadFailure {
    match error {
        TrustedPathError::Missing => ReadFailure::pre("marker_missing"),
        TrustedPathError::Invalid => ReadFailure::pre("marker_invalid"),
        TrustedPathError::Refused(component) => {
            ReadFailure::pre_exact("marker_invalid", component.message())
        }
    }
}

fn app_path_and_home() -> Result<(PathBuf, PathBuf), TrustedPathError> {
    let home = dirs::home_dir().ok_or(TrustedPathError::Invalid)?;
    if !home.is_absolute() {
        return Err(TrustedPathError::Invalid);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    Ok((base.join(crate::session::APP_DIR_NAME_XDG), home))
}

pub(crate) fn existing_app_namespace() -> Result<OwnedNamespace, TrustedPathError> {
    let (path, home) = app_path_and_home()?;
    let euid = crate::process::effective_uid();
    let dir = match open_trusted_directory(&path, euid) {
        Ok(dir) => dir,
        Err(TrustedPathError::Refused(_)) if namespace_is_unpublished(&path, euid) => {
            return Err(TrustedPathError::Missing);
        }
        Err(error) => return Err(error),
    };
    Ok(OwnedNamespace {
        name: crate::server::runtime_ws::NAMESPACE.to_string(),
        home,
        dir,
    })
}

// Observes absence only; this descriptor can never authorize a daemon connection.
fn namespace_is_unpublished(path: &Path, euid: u32) -> bool {
    let dir = match walk_directory(path, euid, false) {
        Ok(dir) => dir,
        Err(TrustedPathError::Missing) => return true,
        Err(_) => return false,
    };
    let Ok(path) = anchored_child_path(dir.as_raw_fd(), ".") else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.starts_with(b"runtime.")
            || bytes.starts_with(b".runtime.")
            || bytes.starts_with(b"lifetime.lock")
            || bytes.starts_with(b"publisher.lock")
        {
            return false;
        }
    }
    true
}

pub(crate) fn open_trusted_directory(path: &Path, euid: u32) -> Result<OwnedFd, TrustedPathError> {
    walk_directory(path, euid, true)
}

fn walk_directory(path: &Path, euid: u32, admission: bool) -> Result<OwnedFd, TrustedPathError> {
    if !path.is_absolute() {
        return Err(TrustedPathError::Invalid);
    }
    let root = open_dir(Path::new("/"))?;
    if admission {
        validate_directory_stat(&root, euid, false, true, Path::new("/"))?;
    }
    let components: Vec<CString> = path
        .components()
        .skip(1)
        .map(|component| {
            let component = component.as_os_str();
            CString::new(component.as_bytes()).map_err(|_| TrustedPathError::Invalid)
        })
        .collect::<Result<_, _>>()?;
    if components.is_empty() {
        return Err(TrustedPathError::Invalid);
    }

    let mut current = root;
    let count = components.len();
    // Keep the encountered spelling for actionable refusals.
    let mut walked = PathBuf::from("/");
    for (index, component) in components.into_iter().enumerate() {
        let final_component = index + 1 == count;
        walked.push(Path::new(OsStr::from_bytes(component.as_bytes())));
        // A missing ancestor is namespace absence; other open failures remain refusals.
        let next =
            open_dir_at(current.as_raw_fd(), &component, final_component).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    TrustedPathError::Missing
                } else {
                    unopenable_component(&walked, &error)
                }
            })?;
        // Connection trust is checked on opened objects, never on their pathnames.
        if admission {
            validate_directory_stat(&next, euid, final_component, true, &walked)?;
        }
        current = next;
    }
    Ok(current)
}

/// Refuse an existing path component with an actionable repair.
fn unopenable_component(path: &Path, error: &std::io::Error) -> TrustedPathError {
    let (cause, remedy) = match os::directory_open_failure(error) {
        os::DirectoryOpenFailure::Symlink => (
            "a symlink, and the app directory may not be one",
            "rm {path} && mkdir -p {path}",
        ),
        os::DirectoryOpenFailure::NotDirectory => (
            "not a directory, where the walk needs one",
            "rm {path} && mkdir -p {path}",
        ),
        os::DirectoryOpenFailure::Permission => (
            "not readable or searchable by this user",
            "chmod u+rwx {path}",
        ),
        os::DirectoryOpenFailure::Other => (
            "not a directory this user may open",
            "Run as the directory's owner, or select HOME/XDG_CONFIG_HOME for the intended user.",
        ),
    };
    // Report the mode of the path the operator can inspect.
    let mode = std::fs::symlink_metadata(path)
        .map(|meta| os::metadata_mode(&meta) & 0o7777)
        .unwrap_or(0);
    TrustedPathError::Refused(RefusedComponent::new(path, mode, cause, remedy))
}

fn open_dir(path: &Path) -> Result<OwnedFd, TrustedPathError> {
    os::open_directory(path).map_err(|_| TrustedPathError::Invalid)
}

fn open_dir_at(
    parent: RawFd,
    component: &CString,
    final_component: bool,
) -> std::io::Result<OwnedFd> {
    os::open_directory_at(parent, component, final_component)
}

fn validate_directory_owner(
    stat: &os::Stat,
    euid: u32,
    final_component: bool,
    path: &Path,
) -> Result<(), TrustedPathError> {
    let mode = stat.st_mode & 0o7777;
    if !directory_stat(stat) {
        return Err(TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "not a directory",
            "Select a real directory at {path}; inspect the existing path before replacing it.",
        )));
    }
    if stat.st_mode & 0o111 == 0 {
        return Err(TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "not searchable by its own owner",
            "chmod u+x {path}",
        )));
    }
    let owned = if final_component {
        stat.st_uid == euid
    } else {
        stat.st_uid == 0 || stat.st_uid == euid
    };
    if !owned {
        return Err(TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "owned by another user",
            "Run as the directory's owner, or select HOME/XDG_CONFIG_HOME for the intended user.",
        )));
    }
    Ok(())
}

/// Verify the opened directory; retain the encountered spelling for diagnostics.
fn validate_directory_stat(
    file: &OwnedFd,
    euid: u32,
    final_component: bool,
    root_check: bool,
    path: &Path,
) -> Result<(), TrustedPathError> {
    let stat = fstat(file.as_raw_fd())?;
    let mode = stat.st_mode & 0o7777;
    if !directory_stat(&stat) {
        return Err(TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "not a directory",
            "rm {path} && mkdir -p {path}",
        )));
    }
    if !group_other_writes_allowed(&stat, final_component) {
        return Err(TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "writable by its group or by others",
            "chmod go-w {path}",
        )));
    }
    if root_check {
        validate_directory_owner(&stat, euid, final_component, path)?;
    }
    // Distinguish a named write from an ACL that cannot be verified.
    validate_posix_acl(file.as_raw_fd()).map_err(|cause| {
        let message = match cause {
            AclError::NamedWrite => {
                "carrying a POSIX ACL that grants a named user or group write access"
            }
            AclError::Unverifiable => "carrying a POSIX ACL whose entries could not be read",
        };
        TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            message,
            "setfacl -b {path}",
        ))
    })?;
    Ok(())
}

/// Only root-owned sticky ancestors may be writable; the app directory stays private.
fn group_other_writes_allowed(stat: &os::Stat, final_component: bool) -> bool {
    stat.st_mode & 0o022 == 0
        || (!final_component
            && stat.st_uid == 0
            && stat.st_mode & crate::process::runtime_io::STICKY != 0)
}

async fn connect_admission(
    namespace: OwnedNamespace,
    exchange_deadline: Instant,
) -> Result<UdsConnection, ReadFailure> {
    let dir = namespace.dir.as_raw_fd();
    let euid = crate::process::effective_uid();
    // Structural and process-start validation precedes the lock and the
    // marker_missing shortcut, so retained crash state is never hidden.
    inspect_temporary_markers(dir)?;
    let (lock, lock_identity) = match open_entry(dir, LOCK_FILE) {
        Ok(value) => value,
        Err(EntryError::Missing) if runtime_entry_present(dir) => {
            return Err(ReadFailure::pre("marker_identity"));
        }
        Err(EntryError::Missing) => return Err(absent_publication(dir, euid)),
        Err(EntryError::Invalid) => return Err(ReadFailure::pre("marker_invalid")),
    };
    validate_regular_file(&lock, euid, &lock_identity)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    if fs2::FileExt::try_lock_shared(&lock).is_err() {
        return Err(ReadFailure::pre("marker_identity"));
    }
    let after = fstatat(dir, LOCK_FILE).map_err(|_| ReadFailure::pre("marker_identity"))?;
    if identity(&after) != lock_identity {
        return Err(ReadFailure::pre("marker_identity"));
    }
    let admission = Admission {
        _namespace: namespace,
        _lock: lock,
    };

    let prebind: Option<PrebindMarker> = match read_marker::<PrebindMarker>(dir, PREBIND_FILE) {
        Ok(value) => {
            validate_prebind(&value, &admission._namespace.name)?;
            Some(value)
        }
        Err(error) if error.code() == "marker_missing" => None,
        Err(error) => return Err(error),
    };
    let postbind: Option<PostbindMarker> = match read_marker::<PostbindMarker>(dir, POSTBIND_FILE) {
        Ok(value) => {
            validate_postbind(&value, &admission._namespace.name)?;
            Some(value)
        }
        Err(error) if error.code() == "marker_missing" => None,
        Err(error) => return Err(error),
    };
    let Some(postbind) = postbind else {
        // A pending owner is not absence, including while it waits for old readers.
        if let Some(prebind) = &prebind {
            if let ProcessState::Dead = process_state(prebind.pid, &prebind.process_start_identity)?
            {
                return Err(absent_publication(dir, euid));
            }
            return Err(ReadFailure::pre("marker_identity"));
        }
        if runtime_entry_present(dir) {
            return Err(ReadFailure::pre("marker_identity"));
        }
        return Err(absent_publication(dir, euid));
    };
    if let Some(prebind) = &prebind {
        if prebind.prebind_instance_id != postbind.prebind_instance_id
            || prebind.pid != postbind.pid
            || prebind.process_start_identity != postbind.process_start_identity
        {
            return Err(ReadFailure::pre("marker_identity"));
        }
    }
    // Dead markers are absence only when no successor owns publication.
    match process_state(postbind.pid, &postbind.process_start_identity)? {
        ProcessState::Live => {}
        ProcessState::Dead => return Err(absent_publication(dir, euid)),
    }
    validate_socket_entry(dir, &postbind, euid)?;

    let path = admission._namespace.anchored_socket_path()?;
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|_| ReadFailure::post("publisher_absent"))?;
    validate_connected_socket(stream.as_raw_fd(), dir, &postbind, euid)?;
    Ok(UdsConnection {
        stream,
        identity: UdsIdentity {
            namespace: admission._namespace.name.clone(),
            prebind_instance_id: postbind.prebind_instance_id,
            runtime_instance_id: postbind.runtime_instance_id,
            runtime_epoch: postbind.runtime_epoch,
            owner_uid: postbind.owner_uid,
        },
        home: admission._namespace.home.clone(),
        admission,
        // Establishment and exchange share one deadline.
        exchange_deadline,
    })
}
fn absent_publication(dir: RawFd, euid: u32) -> ReadFailure {
    match publisher_is_active(dir, euid) {
        Ok(false) => ReadFailure::pre("marker_missing"),
        Ok(true) => ReadFailure::pre("marker_identity"),
        Err(error) => error,
    }
}

fn publisher_is_active(dir: RawFd, euid: u32) -> Result<bool, ReadFailure> {
    let (file, entry) = match open_entry(dir, PUBLISHER_LOCK_FILE) {
        Ok(value) => value,
        Err(EntryError::Missing) => return Ok(false),
        Err(EntryError::Invalid) => return Err(ReadFailure::pre("marker_invalid")),
    };
    validate_regular_file(&file, euid, &entry).map_err(|_| ReadFailure::pre("marker_invalid"))?;
    let active = match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => false,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => return Err(ReadFailure::pre("marker_identity")),
    };
    let after =
        fstatat(dir, PUBLISHER_LOCK_FILE).map_err(|_| ReadFailure::pre("marker_identity"))?;
    if identity(&after) != entry {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(active)
}

fn runtime_entry_present(dir: RawFd) -> bool {
    [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE]
        .iter()
        .any(|name| fstatat(dir, name).is_ok())
}

/// Unsupported schemas are absent; invalid identities or namespaces remain refusals.
fn validate_prebind(marker: &PrebindMarker, namespace: &str) -> Result<(), ReadFailure> {
    if marker.schema != SCHEMA {
        return Err(ReadFailure::pre("marker_missing"));
    }
    if !valid_uuid(&marker.prebind_instance_id) || !valid_namespace(&marker.namespace) {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if marker.namespace != namespace || !valid_process_identity(&marker.process_start_identity) {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(())
}

fn validate_postbind(marker: &PostbindMarker, namespace: &str) -> Result<(), ReadFailure> {
    if marker.schema != SCHEMA {
        return Err(ReadFailure::pre("marker_missing"));
    }
    if !valid_uuid(&marker.prebind_instance_id)
        || !valid_uuid(&marker.runtime_instance_id)
        || !valid_uuid(&marker.runtime_epoch)
        || !valid_namespace(&marker.namespace)
    {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if marker.namespace != namespace
        || marker.socket_path != SOCKET_FILE
        || marker.owner_uid != crate::process::effective_uid()
        || !valid_process_identity(&marker.process_start_identity)
    {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(())
}

fn read_marker<T: for<'de> Deserialize<'de>>(dir: RawFd, name: &str) -> Result<T, ReadFailure> {
    let (file, entry_identity) = open_entry(dir, name).map_err(|error| match error {
        EntryError::Missing => ReadFailure::pre("marker_missing"),
        EntryError::Invalid => ReadFailure::pre("marker_invalid"),
    })?;
    validate_regular_file(&file, crate::process::effective_uid(), &entry_identity)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    if bytes.len() as u64 > MARKER_LIMIT {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    let after = fstatat(dir, name).map_err(|_| ReadFailure::pre("marker_identity"))?;
    if identity(&after) != entry_identity {
        return Err(ReadFailure::pre("marker_identity"));
    }
    serde_json::from_slice(&bytes).map_err(|_| ReadFailure::pre("marker_invalid"))
}

/// Validate all temporaries; dead and incomplete writes do not establish a publisher.
fn inspect_temporary_markers(dir: RawFd) -> Result<(), ReadFailure> {
    let entries = os::read_directory(dir).map_err(|_| ReadFailure::pre("marker_identity"))?;
    for entry in entries {
        let entry = entry.map_err(|_| ReadFailure::pre("marker_identity"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(kind) = temporary_kind(&name) else {
            if name == PREBIND_FILE || name == POSTBIND_FILE || name == SOCKET_FILE {
                continue;
            }
            if name.starts_with(PREBIND_FILE)
                || name.starts_with(POSTBIND_FILE)
                || name.starts_with(SOCKET_FILE)
            {
                return Err(ReadFailure::pre("marker_invalid"));
            }
            continue;
        };
        let suffix = name
            .rsplit_once(TEMPORARY_SEPARATOR)
            .map(|(_, value)| value)
            .unwrap_or_default();
        if !valid_uuid(suffix) {
            return Err(ReadFailure::pre("marker_invalid"));
        }
        let bytes = read_named_file(dir, &name)?;
        let (content_uuid, pid, process_identity) = match kind {
            TempKind::Prebind => match serde_json::from_slice::<PrebindMarker>(&bytes) {
                Ok(value)
                    if value.schema == SCHEMA
                        && valid_namespace(&value.namespace)
                        && valid_uuid(&value.prebind_instance_id) =>
                {
                    (
                        value.prebind_instance_id,
                        value.pid,
                        value.process_start_identity,
                    )
                }
                Ok(_) => return Err(ReadFailure::pre("marker_invalid")),
                // An incomplete abandoned write establishes no publisher.
                Err(_) => continue,
            },
            TempKind::Postbind => match serde_json::from_slice::<PostbindMarker>(&bytes) {
                Ok(value)
                    if value.schema == SCHEMA
                        && valid_namespace(&value.namespace)
                        && valid_uuid(&value.prebind_instance_id) =>
                {
                    (
                        value.prebind_instance_id,
                        value.pid,
                        value.process_start_identity,
                    )
                }
                Ok(_) => return Err(ReadFailure::pre("marker_invalid")),
                Err(_) => continue,
            },
        };
        if content_uuid != suffix {
            return Err(ReadFailure::pre("marker_invalid"));
        }
        // Unprovable liveness is not absence.
        if !valid_process_identity(&process_identity)
            || process_state(pid, &process_identity)? != ProcessState::Dead
        {
            return Err(ReadFailure::pre("marker_identity"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum TempKind {
    Prebind,
    Postbind,
}

fn temporary_kind(name: &str) -> Option<TempKind> {
    if is_temporary_of(name, PREBIND_FILE) {
        Some(TempKind::Prebind)
    } else if is_temporary_of(name, POSTBIND_FILE) {
        Some(TempKind::Postbind)
    } else {
        None
    }
}

fn read_named_file(dir: RawFd, name: &str) -> Result<Vec<u8>, ReadFailure> {
    let (file, entry) = open_entry(dir, name).map_err(|error| match error {
        EntryError::Missing => ReadFailure::pre("marker_identity"),
        EntryError::Invalid => ReadFailure::pre("marker_invalid"),
    })?;
    validate_regular_file(&file, crate::process::effective_uid(), &entry)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    if bytes.len() as u64 > MARKER_LIMIT {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    let after = fstatat(dir, name).map_err(|error| {
        ReadFailure::pre(if error.kind() == std::io::ErrorKind::NotFound {
            "marker_identity"
        } else {
            "marker_invalid"
        })
    })?;
    if identity(&after) != entry {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(bytes)
}

fn validate_socket_entry(
    dir: RawFd,
    marker: &PostbindMarker,
    euid: u32,
) -> Result<(), ReadFailure> {
    let stat = fstatat(dir, SOCKET_FILE).map_err(|_| ReadFailure::pre("marker_identity"))?;
    if !socket_stat(&stat)
        || stat.st_uid != euid
        || stat.st_dev as u64 != marker.socket_device
        || stat.st_ino as u64 != marker.socket_inode
        || !socket_mode_allowed(&stat, euid)
    {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(())
}

fn validate_connected_socket(
    stream: RawFd,
    dir: RawFd,
    marker: &PostbindMarker,
    euid: u32,
) -> Result<(), ReadFailure> {
    let connected = fstat(stream).map_err(|_| ReadFailure::post("socket_identity"))?;
    let current = fstatat(dir, SOCKET_FILE).map_err(|_| ReadFailure::post("socket_identity"))?;
    if !socket_stat(&connected)
        || !socket_stat(&current)
        || current.st_dev as u64 != marker.socket_device
        || current.st_ino as u64 != marker.socket_inode
    {
        return Err(ReadFailure::post("socket_identity"));
    }
    peer_credentials(stream, marker, euid)
}

fn peer_credentials(stream: RawFd, marker: &PostbindMarker, euid: u32) -> Result<(), ReadFailure> {
    let credentials =
        os::peer_credentials(stream).map_err(|_| ReadFailure::post("peer_identity"))?;
    if credentials.uid != marker.owner_uid
        || credentials.uid != euid
        || credentials.pid as u32 != marker.pid
        || credentials.pid as u32 != marker.socket_creator_pid
    {
        return Err(ReadFailure::post("peer_identity"));
    }
    Ok(())
}

fn anchored_child_path(dir: RawFd, child: &str) -> Result<PathBuf, ReadFailure> {
    os::anchored_path(dir, child).map_err(|_| ReadFailure::pre("anchored_alias_unavailable"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy)]
enum EntryError {
    Missing,
    Invalid,
}

fn open_entry(dir: RawFd, name: &str) -> Result<(File, FileIdentity), EntryError> {
    let before = fstatat(dir, name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EntryError::Missing
        } else {
            EntryError::Invalid
        }
    })?;
    if before.st_nlink != 1 || !regular_stat(&before) {
        return Err(EntryError::Invalid);
    }
    let c_name = CString::new(name).map_err(|_| EntryError::Invalid)?;
    let file = os::open_readonly_at(dir, &c_name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EntryError::Missing
        } else {
            EntryError::Invalid
        }
    })?;
    let opened = fstat(file.as_raw_fd()).map_err(|_| EntryError::Invalid)?;
    let before_identity = identity(&before);
    let opened_identity = identity(&opened);
    if before_identity != opened_identity {
        return Err(EntryError::Invalid);
    }
    Ok((file, before_identity))
}

fn validate_regular_file(
    file: &File,
    euid: u32,
    expected: &FileIdentity,
) -> Result<(), TrustedPathError> {
    let stat = fstat(file.as_raw_fd())?;
    if !regular_stat(&stat)
        || stat.st_uid != euid
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
        || identity(&stat) != *expected
    {
        return Err(TrustedPathError::Invalid);
    }
    Ok(())
}

fn identity(stat: &os::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn fstat(fd: RawFd) -> Result<os::Stat, TrustedPathError> {
    os::stat_file(fd).map_err(|_| TrustedPathError::Invalid)
}
fn fstatat(dir: RawFd, name: &str) -> std::io::Result<os::Stat> {
    os::stat_entry(dir, name)
}

fn regular_stat(stat: &os::Stat) -> bool {
    stat.st_mode & crate::process::runtime_io::KIND_MASK == crate::process::runtime_io::REGULAR
}

fn directory_stat(stat: &os::Stat) -> bool {
    stat.st_mode & crate::process::runtime_io::KIND_MASK == crate::process::runtime_io::DIRECTORY
}

fn socket_stat(stat: &os::Stat) -> bool {
    stat.st_mode & crate::process::runtime_io::KIND_MASK == crate::process::runtime_io::SOCKET
}

fn socket_mode_allowed(stat: &os::Stat, euid: u32) -> bool {
    stat.st_uid == euid && stat.st_mode & 0o777 == 0o600
}

/// Invalid process identities are unprovable, never proof of an absent publisher.
pub(crate) fn valid_process_identity(value: &str) -> bool {
    let Some((platform, version, boot, start)) = parse_process_identity(value) else {
        return false;
    };
    platform == "linux"
        && version == "v1"
        && valid_uuid(boot)
        && !start.is_empty()
        && start.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_process_identity(value: &str) -> Option<(&str, &str, &str, &str)> {
    let (platform, rest) = value.split_once(':')?;
    let (version, rest) = rest.split_once(':')?;
    let (boot, start) = rest.rsplit_once(':')?;
    Some((platform, version, boot, start))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Live,
    Dead,
}

fn process_state(pid: u32, recorded: &str) -> Result<ProcessState, ReadFailure> {
    let Some((platform, _, boot, start)) = parse_process_identity(recorded) else {
        return Err(ReadFailure::pre("marker_identity"));
    };
    if platform != "linux" {
        return Err(ReadFailure::pre("marker_identity"));
    }
    let current_boot = current_boot_id().ok_or_else(|| ReadFailure::pre("marker_identity"))?;
    if boot != current_boot {
        return Ok(ProcessState::Dead);
    }
    match process_start(pid)? {
        Some(current) if current == start => Ok(ProcessState::Live),
        Some(_) => Ok(ProcessState::Dead),
        None => Ok(ProcessState::Dead),
    }
}

fn current_boot_id() -> Option<String> {
    let value = os::boot_id()?;
    valid_uuid(&value).then_some(value)
}
fn process_start(pid: u32) -> Result<Option<String>, ReadFailure> {
    match os::process_start_ticks(pid) {
        os::ProcessStart::Ticks(ticks) => Ok(Some(ticks)),
        os::ProcessStart::Absent => Ok(None),
        os::ProcessStart::Unprovable => Err(ReadFailure::pre("marker_identity")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn a_read_retries_an_observed_publication_then_renders_the_published_session() {
        use crate::cli::runtime_read::{
            exchange_stream, ExpectedPeer, ReadRequestSource, ScopedCommand,
        };
        use std::os::unix::fs::OpenOptionsExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (_base, _env) = crate::server::test_support::trusted_namespace()
            .expect("a private ancestor chain exists");
        let mut row = crate::session::Instance::new("after publication", "/repo");
        row.source_profile = "main".into();
        row.tool = "claude".into();
        row.command = "agent --retry-proof".into();
        row.created_at = "2026-01-02T03:04:05Z".parse().unwrap();
        let id = row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("main", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        let app = crate::session::get_app_dir().unwrap();
        let publishing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(app.join(LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&publishing).unwrap();
        for artifact in [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE] {
            assert!(!app.join(artifact).exists());
        }
        let mut observed = 0;
        let mut server = None;
        let connection = connect_inner(Instant::now() + Duration::from_secs(10), |error| {
            assert_eq!(error.code(), "marker_identity");
            observed += 1;
            assert_eq!(observed, 1);
            fs2::FileExt::unlock(&publishing).unwrap();
            let published = crate::server::runtime_uds::try_publish().unwrap();
            server = Some(tokio::spawn(crate::server::runtime_uds::serve(
                state.clone(),
                published,
            )));
        })
        .await
        .expect("the same invocation retries after publication");
        assert_eq!(observed, 1);
        let exchange = connection
            .upgrade(
                "ws://localhost/api/runtime/ws"
                    .into_client_request()
                    .unwrap(),
            )
            .await
            .unwrap();
        let UdsExchange {
            stream,
            identity,
            home,
            deadline,
            _admission,
        } = exchange;
        let source = ReadRequestSource {
            explicit_url: None,
            env_url: None,
            token: None,
            explicit_profile: Some("main".into()),
            env_profile: None,
        };
        let args = crate::cli::list::ListArgs {
            json: true,
            all: false,
            state: crate::cli::list::StateFilter::All,
        };
        let projection = exchange_stream(
            stream,
            deadline,
            ExpectedPeer::Local(identity),
            Some(&home),
            ScopedCommand::List(&args),
            &source,
        )
        .await
        .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&projection.stdout).unwrap();
        assert_eq!(
            rows,
            serde_json::json!([{
                "id": id, "title": "after publication", "path": "/repo", "group": "",
                "tool": "claude", "command": "agent --retry-proof", "profile": "main",
                "state": "live", "created_at": "2026-01-02T03:04:05Z", "workspace_repos": []
            }])
        );
        drop(_admission);
        state.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server.unwrap())
            .await
            .unwrap()
            .unwrap();
        for artifact in [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE] {
            assert!(!app.join(artifact).exists());
        }
    }

    #[test]
    #[serial_test::parallel]
    fn a_remembered_temporary_that_disappears_is_retryable_but_bad_mode_is_not() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let dir = File::open(directory.path()).unwrap();
        let name =
            format!("{PREBIND_FILE}{TEMPORARY_SEPARATOR}aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let path = directory.path().join(&name);
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_named_file(dir.as_raw_fd(), &name).unwrap(), b"{}");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            read_named_file(dir.as_raw_fd(), &name).unwrap_err().code(),
            "marker_identity"
        );
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            read_named_file(dir.as_raw_fd(), &name).unwrap_err().code(),
            "marker_invalid"
        );
    }

    /// Exercise the Linux ACL byte layout without requiring filesystem ACL support.
    #[test]
    fn a_posix_acl_is_read_in_the_bytes_the_kernel_writes() {
        for admitted in [
            acl_value(&[(ACL_USER_OBJ, 6), (ACL_GROUP_OBJ, 4), (ACL_OTHER, 4)]),
            acl_value(&[
                (ACL_USER_OBJ, 7),
                (ACL_GROUP_OBJ, 5),
                (ACL_MASK, 7),
                (ACL_OTHER, 4),
            ]),
            acl_value(&[
                (ACL_USER_OBJ, 6),
                (ACL_USER, 4),
                (ACL_GROUP_OBJ, 4),
                (ACL_MASK, 4),
                (ACL_OTHER, 0),
            ]),
        ] {
            assert_eq!(
                validate_acl_value(&admitted),
                Ok(()),
                "nobody is named with a write the mode does not already show"
            );
        }
        for (refused, cause) in [
            (
                acl_value(&[
                    (ACL_USER_OBJ, 7),
                    (ACL_USER, 7),
                    (ACL_GROUP_OBJ, 0),
                    (ACL_MASK, 4),
                    (ACL_OTHER, 0),
                ]),
                AclError::NamedWrite,
            ),
            (
                acl_value(&[
                    (ACL_USER_OBJ, 7),
                    (ACL_GROUP, 6),
                    (ACL_GROUP_OBJ, 0),
                    (ACL_MASK, 4),
                    (ACL_OTHER, 0),
                ]),
                AclError::NamedWrite,
            ),
            // Malformed ACLs remain unverifiable rather than claiming a named write.
            (acl_version(3), AclError::Unverifiable),
            (
                acl_value(&[(ACL_USER_OBJ, 6), (0x40, 6), (ACL_OTHER, 4)]),
                AclError::Unverifiable,
            ),
            (
                acl_value(&[(ACL_USER_OBJ, 6), (ACL_OTHER, 4 | 0x08)]),
                AclError::Unverifiable,
            ),
            (
                acl_value(&[(ACL_USER_OBJ, 6)])[..10].to_vec(),
                AclError::Unverifiable,
            ),
            (vec![0x02, 0x00], AclError::Unverifiable),
        ] {
            assert_eq!(
                validate_acl_value(&refused),
                Err(cause),
                "the cause says whether a named write was read or nothing could be read"
            );
        }
    }

    /// The version word followed by one 8-byte entry per tag and permission
    /// pair, in the layout the kernel writes: a `u16` tag, a `u16` of
    /// permissions and a `u32` id.
    fn acl_value(entries: &[(u8, u8)]) -> Vec<u8> {
        let mut value = ACL_VERSION.to_le_bytes().to_vec();
        for (tag, permissions) in entries {
            value.extend_from_slice(&u16::from(*tag).to_le_bytes());
            value.extend_from_slice(&u16::from(*permissions).to_le_bytes());
            value.extend_from_slice(&0u32.to_le_bytes());
        }
        value
    }

    fn acl_version(version: u32) -> Vec<u8> {
        version.to_le_bytes().to_vec()
    }

    #[test]
    fn process_identity_grammar_is_ascii_and_structured() {
        let boot = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert!(valid_process_identity(&format!("linux:v1:{boot}:123")));
        assert!(!valid_process_identity(&format!("linux:v1:{boot}:123 ")));
        assert!(!valid_process_identity(&format!("linux:v2:{boot}:123")));
    }

    #[test]
    #[serial_test::parallel]
    fn an_absent_ancestor_is_an_absence_and_an_untrustworthy_one_is_not() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;

        let euid = crate::process::effective_uid();
        let base = tempfile::tempdir().expect("temp base");
        let absent = base.path().join("never-created").join("app-dir");
        assert!(matches!(
            open_trusted_directory(&absent, euid),
            Err(TrustedPathError::Missing)
        ));

        let present = base.path().join("app-dir");
        std::fs::create_dir(&present).expect("create");
        std::fs::set_permissions(&present, std::fs::Permissions::from_mode(0o700))
            .expect("private app");
        assert!(open_trusted_directory(&present, euid).is_ok());

        let world = base.path().join("world");
        std::fs::create_dir(&world).expect("create");
        std::fs::set_permissions(&world, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let world_child = world.join("app-dir");
        std::fs::create_dir(&world_child).expect("create");
        std::fs::set_permissions(&world_child, std::fs::Permissions::from_mode(0o700))
            .expect("private app");
        assert!(matches!(
            open_trusted_directory(&world_child, euid),
            Err(TrustedPathError::Refused(_))
        ));
    }
    #[test]
    #[serial_test::parallel]
    fn a_foreign_owned_unpublished_namespace_is_absent_but_never_admitted() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let home = tempfile::tempdir().unwrap();
        let app = home.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let caller_uid = if crate::process::effective_uid() == 0 {
            1
        } else {
            0
        };
        assert!(matches!(
            open_trusted_directory(&app, caller_uid),
            Err(TrustedPathError::Refused(_))
        ));
        assert!(namespace_is_unpublished(&app, caller_uid));
        std::fs::write(app.join("runtime.prebind.json"), b"{}").unwrap();
        assert!(!namespace_is_unpublished(&app, caller_uid));
        assert!(matches!(
            open_trusted_directory(&app, caller_uid),
            Err(TrustedPathError::Refused(_))
        ));
    }

    /// Follow prefix symlinks without relaxing descriptor checks or final no-follow.
    #[test]
    #[serial_test::parallel]
    fn a_symlinked_prefix_is_followed_but_every_check_still_applies() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;

        let euid = crate::process::effective_uid();
        let base = tempfile::tempdir().expect("temp base");
        let real = base.path().join("real");
        std::fs::create_dir(&real).expect("real dir");
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("prefix symlink");

        // The app directory reached through the symlink is admitted.
        let app = real.join("app-dir");
        std::fs::create_dir(&app).expect("app dir");
        assert!(
            open_trusted_directory(&link.join("app-dir"), euid).is_ok(),
            "a symlinked prefix must not refuse the walk"
        );

        // The checks are read off the resolved directory, not the link.
        let mut permissions = std::fs::metadata(&app).expect("stat").permissions();
        permissions.set_mode(0o777);
        std::fs::set_permissions(&app, permissions).expect("chmod");
        assert!(
            matches!(
                open_trusted_directory(&link.join("app-dir"), euid),
                Err(TrustedPathError::Refused(_))
            ),
            "a world-writable directory behind a symlink is still refused"
        );
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        // A prefix that is a symlink to a regular file is a non-directory
        // component, not an absence.
        let file_link = base.path().join("file-link");
        let target = base.path().join("not-a-dir");
        std::fs::write(&target, b"x").expect("file");
        std::os::unix::fs::symlink(&target, &file_link).expect("file symlink");
        assert!(matches!(
            open_trusted_directory(&file_link.join("app-dir"), euid),
            Err(TrustedPathError::Refused(_))
        ));

        // And the final component is still pinned by O_NOFOLLOW.
        let app_link = base.path().join("app-link");
        std::os::unix::fs::symlink(&app, &app_link).expect("app symlink");
        assert!(matches!(
            open_trusted_directory(&app_link, euid),
            Err(TrustedPathError::Refused(_))
        ));
    }

    /// Only root-owned sticky ancestors get the writable-directory allowance.
    #[test]
    fn a_sticky_root_ancestor_is_walkable_and_nothing_else_is() {
        let mut stat = os::Stat {
            st_mode: os::DIRECTORY | 0o777,
            st_uid: 0,
            st_dev: 0,
            st_ino: 0,
            st_nlink: 1,
        };

        stat.st_uid = 0;
        assert!(!group_other_writes_allowed(&stat, true));
        stat.st_mode |= crate::process::runtime_io::STICKY;
        assert!(group_other_writes_allowed(&stat, false));
        assert!(!group_other_writes_allowed(&stat, true));

        stat.st_uid = 1;
        assert!(!group_other_writes_allowed(&stat, false));

        stat.st_uid = 0;
        stat.st_mode = crate::process::runtime_io::DIRECTORY | 0o755;
        assert!(group_other_writes_allowed(&stat, false));
        stat.st_mode = crate::process::runtime_io::DIRECTORY | 0o757;
        assert!(!group_other_writes_allowed(&stat, false));
        stat.st_uid = 1;
        stat.st_mode = os::DIRECTORY | 0o700;
        assert!(matches!(
            validate_directory_owner(&stat, 0, true, Path::new("/preserved-home")),
            Err(TrustedPathError::Refused(_))
        ));
    }

    #[test]
    fn temporary_name_requires_canonical_uuid() {
        let name = "runtime.prebind.json.tmp.aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert!(temporary_kind(name).is_some());
        assert!(valid_uuid(name.rsplit_once(".tmp.").unwrap().1));
        assert!(!valid_uuid("AAAAAAAA-bbbb-cccc-dddd-eeeeeeeeeeee"));
    }

    #[tokio::test]
    #[serial_test::parallel]
    async fn abandoned_temporaries_do_not_establish_a_publication() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;
        let instance = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let prebind = serde_json::json!({
            "schema": SCHEMA, "pid": u32::MAX,
            "process_start_identity": "linux:v1:00000000-0000-0000-0000-000000000000:1",
            "prebind_instance_id": instance, "namespace": "debug:agent-of-empires-dev",
        });
        let mut postbind = prebind.clone();
        for (key, value) in [
            ("runtime_instance_id", serde_json::json!(instance)),
            ("runtime_epoch", serde_json::json!(instance)),
            ("socket_path", serde_json::json!(SOCKET_FILE)),
            (
                "owner_uid",
                serde_json::json!(crate::process::effective_uid()),
            ),
            ("socket_device", serde_json::json!(1)),
            ("socket_inode", serde_json::json!(1)),
            ("socket_creator_pid", serde_json::json!(u32::MAX)),
        ] {
            postbind[key] = value;
        }
        for with_lock in [false, true] {
            for (name, bytes) in [
                (PREBIND_FILE, None),
                (PREBIND_FILE, Some(Vec::new())),
                (PREBIND_FILE, Some(serde_json::to_vec(&prebind).unwrap())),
                (POSTBIND_FILE, Some(serde_json::to_vec(&postbind).unwrap())),
            ] {
                let dir = tempfile::tempdir().unwrap();
                if with_lock {
                    let lock = dir.path().join(LOCK_FILE);
                    std::fs::write(&lock, b"").unwrap();
                    std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o600)).unwrap();
                }
                if let Some(bytes) = bytes {
                    let temporary = dir.path().join(format!("{name}.tmp.{instance}"));
                    std::fs::write(&temporary, bytes).unwrap();
                    std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                let namespace = OwnedNamespace {
                    name: "debug:agent-of-empires-dev".into(),
                    home: dir.path().to_path_buf(),
                    dir: OwnedFd::from(File::open(dir.path()).unwrap()),
                };
                let Err(error) = connect_admission(
                    namespace,
                    Instant::now() + crate::server::runtime_ws::CONNECTION_BUDGET,
                )
                .await
                else {
                    panic!("no publication can be admitted");
                };
                assert_eq!(error.code(), "marker_missing", "{name}, lock={with_lock}");
            }
        }
    }

    /// Proven-dead markers permit absence without spending the retry budget.
    #[tokio::test]
    #[serial_test::parallel]
    async fn a_dead_publisher_is_an_absence_rather_than_a_retryable_identity() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("namespace");
        let lock = dir.path().join(LOCK_FILE);
        std::fs::write(&lock, b"").expect("lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let dead_pid = 4_194_302u32;
        let identity = format!(
            "linux:v1:{}:1",
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .expect("boot id")
                .trim()
        );
        let instance = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
        let namespace_name = "debug:agent-of-empires-dev";
        let prebind = serde_json::json!({
            "schema": 1,
            "pid": dead_pid,
            "process_start_identity": identity,
            "prebind_instance_id": instance,
            "namespace": namespace_name,
        });
        let postbind = serde_json::json!({
            "schema": 1,
            "pid": dead_pid,
            "process_start_identity": identity,
            "prebind_instance_id": instance,
            "runtime_instance_id": "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
            "runtime_epoch": "3f2b1c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5e",
            "namespace": namespace_name,
            "socket_path": SOCKET_FILE,
            "owner_uid": crate::process::effective_uid(),
            "socket_device": 1,
            "socket_inode": 1,
            "socket_creator_pid": dead_pid,
        });
        for (name, marker) in [(PREBIND_FILE, &prebind), (POSTBIND_FILE, &postbind)] {
            let path = dir.path().join(name);
            std::fs::write(&path, marker.to_string()).expect("marker");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("marker mode");
        }

        let opened = std::fs::File::open(dir.path()).expect("open namespace");
        let namespace = OwnedNamespace {
            name: namespace_name.to_string(),
            home: dir.path().to_path_buf(),
            dir: OwnedFd::from(opened),
        };
        let Err(error) = connect_admission(
            namespace,
            Instant::now() + crate::server::runtime_ws::CONNECTION_BUDGET,
        )
        .await
        else {
            panic!("a dead publisher admits nothing");
        };
        assert_eq!(
            error.code(),
            "marker_missing",
            "a provably dead publisher is an absence, not a retryable identity fault"
        );
    }

    #[tokio::test]
    #[serial_test::parallel]
    async fn an_admitted_publisher_that_never_completes_the_exchange_is_an_absence() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use tokio::net::UnixListener;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let dir = tempfile::tempdir().expect("namespace");
        let socket_path = dir.path().join(SOCKET_FILE);
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let lock = dir.path().join(LOCK_FILE);
        std::fs::write(&lock, b"").expect("lock");
        let request = || {
            "ws://localhost/api/runtime/ws"
                .into_client_request()
                .expect("the local request")
        };

        let Err(error) = admitted_connection(dir.path(), &socket_path, &lock, Instant::now())
            .await
            .upgrade(request())
            .await
        else {
            panic!("a publisher that never answers admits no exchange");
        };
        assert_eq!(
            error.code(),
            "publisher_absent",
            "a spent budget against a silent publisher is a statement about the environment"
        );

        // Drain the expired attempt before connecting the peer-drop case.
        drop(listener.accept().await.expect("expired attempt queued").0);
        let connection = admitted_connection(
            dir.path(),
            &socket_path,
            &lock,
            Instant::now() + crate::server::runtime_ws::CONNECTION_BUDGET,
        )
        .await;
        let (publisher, _) = listener.accept().await.expect("peer-drop attempt queued");
        drop(publisher);
        let Err(error) =
            tokio::time::timeout(Duration::from_secs(2), connection.upgrade(request()))
                .await
                .expect("the dropped peer must finish before the exchange deadline")
        else {
            panic!("a publisher that drops the exchange admits none");
        };
        assert_eq!(
            error.code(),
            "publisher_absent",
            "an admitted connection dropped mid-handshake is the same absence"
        );
    }

    /// Bypass marker admission to exercise peers that stop before handshake.
    async fn admitted_connection(
        dir: &std::path::Path,
        socket_path: &std::path::Path,
        lock: &std::path::Path,
        exchange_deadline: Instant,
    ) -> UdsConnection {
        let opened = File::open(dir).expect("open namespace");
        UdsConnection {
            stream: UnixStream::connect(socket_path)
                .await
                .expect("connect socket"),
            identity: UdsIdentity {
                namespace: "debug:agent-of-empires-dev".into(),
                prebind_instance_id: String::new(),
                runtime_instance_id: String::new(),
                runtime_epoch: String::new(),
                owner_uid: crate::process::effective_uid(),
            },
            home: dir.to_path_buf(),
            admission: Admission {
                _namespace: OwnedNamespace {
                    name: "debug:agent-of-empires-dev".into(),
                    home: dir.to_path_buf(),
                    dir: OwnedFd::from(opened),
                },
                _lock: File::open(lock).expect("lock file"),
            },
            exchange_deadline,
        }
    }

    /// A dead prebind-only publication also permits local takeover.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_half_published_dead_publisher_is_an_absence_too() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("namespace");
        let lock = dir.path().join(LOCK_FILE);
        std::fs::write(&lock, b"").expect("lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let dead_pid = 4_194_302u32;
        let identity = format!(
            "linux:v1:{}:1",
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .expect("boot id")
                .trim()
        );
        let instance = "2c9d1a6e-7f4b-4d2a-9e5c-1b8f0a3d6c72";
        let namespace_name = "debug:agent-of-empires-dev";
        let prebind = serde_json::json!({
            "schema": 1,
            "pid": dead_pid,
            "process_start_identity": identity,
            "prebind_instance_id": instance,
            "namespace": namespace_name,
        });
        let path = dir.path().join(PREBIND_FILE);
        std::fs::write(&path, prebind.to_string()).expect("prebind");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::fs::write(dir.path().join(SOCKET_FILE), b"").expect("socket");

        let opened = std::fs::File::open(dir.path()).expect("open namespace");
        let namespace = OwnedNamespace {
            name: namespace_name.to_string(),
            home: dir.path().to_path_buf(),
            dir: OwnedFd::from(opened),
        };
        let Err(error) = connect_admission(
            namespace,
            Instant::now() + crate::server::runtime_ws::CONNECTION_BUDGET,
        )
        .await
        else {
            panic!("a half-published dead publisher admits nothing");
        };
        assert_eq!(
            error.code(),
            "marker_missing",
            "a half-published dead publisher is an absence, not a retryable identity"
        );
    }

    #[test]
    #[serial_test::parallel]
    fn connected_socket_identity_does_not_require_path_inode_equality() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = tempfile::tempdir().expect("temp namespace");
        let socket_path = dir.path().join(SOCKET_FILE);
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let client = UnixStream::connect(&socket_path).expect("connect socket");
        let (server, _) = listener.accept().expect("accept socket");
        let dir_file = File::open(dir.path()).expect("open namespace");
        let path_stat = fstatat(dir_file.as_raw_fd(), SOCKET_FILE).expect("stat socket");
        let marker = PostbindMarker {
            schema: SCHEMA,
            pid: std::process::id(),
            process_start_identity: String::new(),
            prebind_instance_id: String::new(),
            runtime_instance_id: String::new(),
            runtime_epoch: String::new(),
            namespace: String::new(),
            socket_path: SOCKET_FILE.into(),
            owner_uid: crate::process::effective_uid(),
            socket_device: path_stat.st_dev as u64,
            socket_inode: path_stat.st_ino as u64,
            socket_creator_pid: std::process::id(),
        };

        assert!(validate_connected_socket(
            server.as_raw_fd(),
            dir_file.as_raw_fd(),
            &marker,
            crate::process::effective_uid(),
        )
        .is_ok());
        drop(client);
    }

    /// Unsupported schemas and malformed bodies have distinct refusal codes.
    #[test]
    fn a_marker_schema_and_a_marker_body_are_two_different_refusals() {
        let namespace = crate::server::runtime_ws::NAMESPACE;
        let good_uuid = "11111111-2222-3333-4444-555555555555";
        let identity = "linux:v1:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee:1";
        let prebind = |schema: u8, instance: &str| PrebindMarker {
            schema,
            pid: std::process::id(),
            process_start_identity: identity.to_string(),
            prebind_instance_id: instance.to_string(),
            namespace: namespace.to_string(),
        };
        let postbind = |schema: u8, instance: &str| PostbindMarker {
            schema,
            pid: std::process::id(),
            process_start_identity: identity.to_string(),
            prebind_instance_id: instance.to_string(),
            runtime_instance_id: "22222222-2222-3333-4444-555555555555".to_string(),
            runtime_epoch: "33333333-2222-3333-4444-555555555555".to_string(),
            namespace: namespace.to_string(),
            socket_path: SOCKET_FILE.to_string(),
            owner_uid: crate::process::effective_uid(),
            socket_device: 1,
            socket_inode: 1,
            socket_creator_pid: std::process::id(),
        };
        let bad_uuid = "not-a-uuid";
        let cases: [(u8, &str, &str); 2] = [
            (SCHEMA + 1, good_uuid, "marker_missing"),
            (SCHEMA, bad_uuid, "marker_invalid"),
        ];
        for (schema, instance, code) in cases {
            assert_eq!(
                validate_prebind(&prebind(schema, instance), namespace)
                    .expect_err("schema {schema} with {instance}")
                    .code(),
                code
            );
            assert_eq!(
                validate_postbind(&postbind(schema, instance), namespace)
                    .expect_err("schema {schema} with {instance}")
                    .code(),
                code
            );
        }
    }
}

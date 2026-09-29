use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::net::UnixStream;
use tokio::time::Instant;

use super::dto::{valid_namespace, valid_uuid};
use super::ReadFailure;

const LOCK_FILE: &str = "lifetime.lock";
const PREBIND_FILE: &str = "runtime.prebind.json";
const POSTBIND_FILE: &str = "runtime.postbind.json";
const SOCKET_FILE: &str = "runtime.sock";
const MARKER_LIMIT: u64 = 64 * 1024;
/// How long the client waits before re-admitting after a republication it
/// caught mid-flight. Short enough that a read which raced a publication is
/// indistinguishable from one that did not, long enough not to spin on a
/// namespace that is genuinely being rewritten.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrustedPathError {
    Missing,
    Invalid,
    /// A component of the walk that resolved and was still refused, with
    /// everything the refusal has to say about it. The variant exists so the
    /// operator is told *which* directory and *what* to do about it: the
    /// refusal itself is unchanged, it only stops being silent.
    Refused(RefusedComponent),
}

/// One refused directory of the walk, and the sentence the operator gets.
///
/// The path is the component as the walk reached it, the mode is the one read
/// off the descriptor, and the remedy is the command that clears this
/// particular cause. Both are static because they are the walk's own rules:
/// there is a closed set of reasons a component can be refused for, and each
/// one has one fix.
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

    /// The refusal as the operator reads it: what was refused, what is wrong
    /// with it, and the one command that fixes it. The mode is printed as
    /// octal because that is the only spelling `chmod` accepts.
    fn message(&self) -> String {
        format!(
            "refused to read the daemon's runtime state: {} is {} (mode {}).\n\
             Fix it with: {}\n",
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
        .map_err(|_| ReadFailure::post("connection_closed"))?
        .map_err(|_| ReadFailure::post("unavailable"))?;
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

/// Admit the local read, retrying for as long as the establishment budget
/// lasts.
///
/// A single attempt turns a publication race into a refusal: a daemon that
/// republishes between the client's walk and its marker read leaves the
/// markers describing a process that is no longer the one holding the socket,
/// which is `marker_identity`: a true statement about a state that lasts
/// microseconds. So that code alone is re-admitted rather than returned, and
/// only an exhausted budget turns the last refusal into the answer. Every
/// other code is final, including the `marker_invalid` that says the artifacts
/// are present but not trustworthy, and including `marker_missing`, which is
/// the one refusal the local command path is allowed to take over: it now
/// covers a provably dead publisher as well as an empty namespace, so "no
/// daemon is publishing here".
pub(crate) async fn connect(establishment_deadline: Instant) -> Result<UdsConnection, ReadFailure> {
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
        // The same code and the same exit, with the operator's own sentence
        // under them: the walk is not admitting anything it refused before, it
        // is only saying which directory and which fix.
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
    let euid = unsafe { libc::geteuid() };
    let dir = open_trusted_directory(&path, euid)?;
    let name = if cfg!(debug_assertions) {
        "debug:agent-of-empires-dev"
    } else {
        "release:agent-of-empires"
    };
    Ok(OwnedNamespace {
        name: name.to_string(),
        home,
        dir,
    })
}

pub(crate) fn open_trusted_directory(path: &Path, euid: u32) -> Result<OwnedFd, TrustedPathError> {
    if !path.is_absolute() {
        return Err(TrustedPathError::Invalid);
    }
    let root = open_dir(Path::new("/"))?;
    validate_directory_stat(&root, euid, false, true, Path::new("/"))?;
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
    // The path of the component the cursor is on, rebuilt as the walk
    // descends, so a refusal names the directory that was refused rather than
    // the walk it was refused in.
    let mut walked = PathBuf::from("/");
    for (index, component) in components.into_iter().enumerate() {
        let final_component = index + 1 == count;
        walked.push(Path::new(OsStr::from_bytes(component.as_bytes())));
        // A component that does not exist means the app directory does not
        // exist, wherever in the chain it is: a fresh home has no
        // `~/.local` to hold one. Every other errno keeps its own class, so
        // an unreadable or non-directory component is still a refusal rather
        // than an absence.
        let next =
            open_dir_at(current.as_raw_fd(), &component, final_component).map_err(|error| {
                if error.raw_os_error() == Some(libc::ENOENT) {
                    TrustedPathError::Missing
                } else {
                    unopenable_component(&walked, &error)
                }
            })?;
        // Every component that resolves is verified by descriptor, whichever
        // component of the path it was reached through: ownership, the
        // group/other-write allowance, the sticky-root ancestor rule and the
        // POSIX ACL are all read off the opened directory, never off the
        // path.
        validate_directory_stat(&next, euid, final_component, true, &walked)?;
        current = next;
    }
    Ok(current)
}

/// A component that exists but could not be opened is a refusal, and the
/// errno says which one: each of these has its own fix, and the operator is
/// the one who has to apply it.
fn unopenable_component(path: &Path, error: &std::io::Error) -> TrustedPathError {
    let (cause, remedy) = match error.raw_os_error() {
        Some(libc::ELOOP) => (
            "a symlink, and the app directory may not be one",
            "rm {path} && mkdir -p {path}",
        ),
        Some(libc::ENOTDIR) => (
            "not a directory, where the walk needs one",
            "rm {path} && mkdir -p {path}",
        ),
        Some(libc::EACCES) | Some(libc::EPERM) => (
            "not readable or searchable by this user",
            "chmod u+rwx {path}",
        ),
        _ => (
            "not a directory this user may open",
            "chown $(id -u) {path} && chmod u+rwx {path}",
        ),
    };
    // The mode is read from the name itself, which is what the operator sees
    // when they run `ls -ld`; a name that resolves to nothing usable has no
    // better spelling than that.
    let mode = std::fs::symlink_metadata(path)
        .map(|meta| meta.permissions().mode() & 0o7777)
        .unwrap_or(0);
    TrustedPathError::Refused(RefusedComponent::new(path, mode, cause, remedy))
}

fn open_dir(path: &Path) -> Result<OwnedFd, TrustedPathError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| TrustedPathError::Invalid)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    fd_to_owned(fd).map_err(|_| TrustedPathError::Invalid)
}

/// Open one component of the walk.
///
/// `O_NOFOLLOW` guards the final component only, the same convention the hook
/// guard uses (`src/hooks/dir_guard.rs`): a prefix symlink: a home reached
/// through one, or macOS `/tmp` → `/private/tmp`: is followed and the
/// directory it resolves to is verified by descriptor, which is where the
/// ownership, mode, sticky-root and ACL checks are read from. A symlinked
/// *final* component stays a refusal, because that would let the app directory
/// itself be swapped for an attacker-chosen inode.
fn open_dir_at(
    parent: RawFd,
    component: &CString,
    final_component: bool,
) -> std::io::Result<OwnedFd> {
    let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    if final_component {
        flags |= libc::O_NOFOLLOW;
    }
    let fd = unsafe { libc::openat(parent, component.as_ptr(), flags) };
    fd_to_owned(fd)
}

fn fd_to_owned(fd: RawFd) -> std::io::Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Verify one component of the walk by descriptor, naming the component and
/// the fix in every refusal it raises.
///
/// `path` is the component as the walk reached it, which is what the operator
/// can act on: the same directory reached through a symlinked prefix is
/// reported at the path the walk took, because that is the one they can
/// `chmod`.
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
        let searchable = stat.st_mode & 0o111 != 0;
        if !searchable {
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
                "chown $(id -u) {path}",
            )));
        }
    }
    // The ACL check reports a named user or group holding a write the mode
    // does not show, so the mode alone cannot explain this refusal; the
    // component and its mode are still printed, because they are what tells
    // the operator which directory now carries one.
    validate_posix_acl(file.as_raw_fd()).map_err(|_| {
        TrustedPathError::Refused(RefusedComponent::new(
            path,
            mode,
            "carrying a POSIX ACL that grants a named user or group write access",
            "setfacl -b {path}",
        ))
    })?;
    Ok(())
}

/// Whether group or other write bits are tolerable on this component of the
/// walk. A non-final ancestor is tolerable when it is root-owned and sticky:
/// a stranger may create entries but may not rename or replace one that
/// belongs to somebody else, which is what `/tmp` offers and what a test
/// namespace lives under. The final component has no such protection, and a
/// sticky one would let its own owner be replaced, so it stays private.
fn group_other_writes_allowed(stat: &libc::stat, final_component: bool) -> bool {
    stat.st_mode & 0o022 == 0
        || (!final_component && stat.st_uid == 0 && stat.st_mode & libc::S_ISVTX != 0)
}

fn validate_posix_acl(fd: RawFd) -> Result<(), TrustedPathError> {
    let names = xattr_names(fd)?;
    let Some(name) = names
        .into_iter()
        .find(|name| name.as_slice() == b"system.posix_acl_access")
    else {
        return Ok(());
    };
    validate_acl_value(&xattr_value(fd, &name)?)
}

fn xattr_names(fd: RawFd) -> Result<Vec<Vec<u8>>, TrustedPathError> {
    let needed = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if needed < 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOENT)) {
            Ok(Vec::new())
        } else {
            Err(TrustedPathError::Invalid)
        };
    }
    if needed == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0u8; needed as usize];
    let actual = unsafe { libc::flistxattr(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if actual < 0 {
        return Err(TrustedPathError::Invalid);
    }
    bytes.truncate(actual as usize);
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| name.to_vec())
        .collect())
}

fn xattr_value(fd: RawFd, name: &[u8]) -> Result<Vec<u8>, TrustedPathError> {
    let c_name = CString::new(name).map_err(|_| TrustedPathError::Invalid)?;
    let needed = unsafe { libc::fgetxattr(fd, c_name.as_ptr(), std::ptr::null_mut(), 0) };
    if needed < 0 {
        return Err(TrustedPathError::Invalid);
    }
    let mut value = vec![0u8; needed as usize];
    let actual =
        unsafe { libc::fgetxattr(fd, c_name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
    if actual < 0 {
        return Err(TrustedPathError::Invalid);
    }
    value.truncate(actual as usize);
    Ok(value)
}

/// The kernel serves `system.posix_acl_access` in its text form, one entry per
/// comma, each `tag[:id]:perms` with `perms` spelled in `rwx` letters: for
/// example `u::rw-,g::r--,o::r--,u:1002:rwx`. Only a *named* user or group
/// holding `w` is refused here: the base owner, group and other entries are
/// already reflected in the directory mode, and the mask entry applies to
/// them. Anything that does not parse is a refusal, so a value this code
/// cannot read is never treated as a chain it may trust.
fn validate_acl_value(value: &[u8]) -> Result<(), TrustedPathError> {
    let text = std::str::from_utf8(value).map_err(|_| TrustedPathError::Invalid)?;
    for entry in text.split(',') {
        let mut fields = entry.split(':');
        let tag = fields.next().ok_or(TrustedPathError::Invalid)?;
        let id = fields.next().unwrap_or_default();
        let permissions = fields.next().ok_or(TrustedPathError::Invalid)?;
        if fields.next().is_some() || !matches!(tag, "u" | "g" | "o" | "m") {
            return Err(TrustedPathError::Invalid);
        }
        // Permission letters are `rwx` with a dash standing in for an absent
        // bit, so `r-x` and `rw-` are spellings, not separators.
        if permissions.is_empty()
            || permissions.len() > 3
            || !permissions
                .chars()
                .all(|permission| matches!(permission, 'r' | 'w' | 'x' | '-'))
        {
            return Err(TrustedPathError::Invalid);
        }
        if id.is_empty() {
            // A base entry: the directory mode already carries it.
            continue;
        }
        if !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(TrustedPathError::Invalid);
        }
        if permissions.contains('w') {
            return Err(TrustedPathError::Invalid);
        }
    }
    Ok(())
}

async fn connect_admission(
    namespace: OwnedNamespace,
    exchange_deadline: Instant,
) -> Result<UdsConnection, ReadFailure> {
    let dir = namespace.dir.as_raw_fd();
    let euid = unsafe { libc::geteuid() };
    // Structural and process-start validation precedes the lock and the
    // marker_missing shortcut, so retained crash state is never hidden.
    inspect_temporary_markers(dir)?;
    let (lock, lock_identity) = match open_entry(dir, LOCK_FILE) {
        Ok(value) => value,
        Err(EntryError::Missing) if runtime_entry_present(dir) => {
            return Err(ReadFailure::pre("marker_identity"));
        }
        Err(EntryError::Missing) => return Err(ReadFailure::pre("marker_missing")),
        Err(EntryError::Invalid) => return Err(ReadFailure::pre("marker_invalid")),
    };
    validate_regular_file(&lock, euid, &lock_identity)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;

    let lock_fd = lock.as_raw_fd();
    if unsafe { libc::flock(lock_fd, libc::LOCK_SH | libc::LOCK_NB) } != 0 {
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
        // A publication killed part-way leaves the prebind and the socket with
        // no postbind, and only a daemon that starts again reaps that shape.
        // The prebind already carries the pid and its start identity, so the
        // same liveness proof that makes a complete dead pair an absence is
        // available here: a provably dead publisher is absent and the local
        // store answers, while a live one is mid-publication and keeps its
        // retry, and an unreadable `/proc` stays `marker_identity` because
        // that is a statement about liveness this client cannot make.
        if let Some(prebind) = &prebind {
            if let ProcessState::Dead = process_state(prebind.pid, &prebind.process_start_identity)?
            {
                return Err(ReadFailure::pre("marker_missing"));
            }
            return Err(ReadFailure::pre("marker_identity"));
        }
        if runtime_entry_present(dir) {
            return Err(ReadFailure::pre("marker_identity"));
        }
        return Err(ReadFailure::pre("marker_missing"));
    };
    if let Some(prebind) = &prebind {
        if prebind.prebind_instance_id != postbind.prebind_instance_id
            || prebind.pid != postbind.pid
            || prebind.process_start_identity != postbind.process_start_identity
        {
            return Err(ReadFailure::pre("marker_identity"));
        }
    }
    // A publisher that is provably gone is an absence, not a race, and retrying
    // it would spend the whole establishment budget to report the same thing
    // twice. A *live* publisher mid-republication cannot reach this branch: it
    // holds `LOCK_EX` while it writes, so the client's own `flock` above failed
    // first with `marker_identity`. An unprovable state: an unreadable
    // `/proc`, a malformed identity: stays `marker_identity` and keeps its
    // retry, because that is a statement about liveness this client cannot make.
    match process_state(postbind.pid, &postbind.process_start_identity)? {
        ProcessState::Live => {}
        ProcessState::Dead => return Err(ReadFailure::pre("marker_missing")),
    }
    validate_socket_entry(dir, &postbind, euid)?;

    let path = admission._namespace.anchored_socket_path()?;
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|_| ReadFailure::post("unavailable"))?;
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
        // One budget per read: the exchange rides the window the caller opened
        // for establishment rather than a second one behind it.
        exchange_deadline,
    })
}

fn runtime_entry_present(dir: RawFd) -> bool {
    if [PREBIND_FILE, POSTBIND_FILE, SOCKET_FILE]
        .iter()
        .any(|name| fstatat(dir, name).is_ok())
    {
        return true;
    }
    directory_contains_temp_marker(dir)
}

fn directory_contains_temp_marker(dir: RawFd) -> bool {
    let duplicate = unsafe { libc::dup(dir) };
    let Ok(scan) = fd_to_owned(duplicate) else {
        return true;
    };
    let raw = scan.into_raw_fd();
    let entries = unsafe { libc::fdopendir(raw) };
    if entries.is_null() {
        unsafe { libc::close(raw) };
        return true;
    }
    let mut found = false;
    loop {
        errno_reset();
        let entry = unsafe { libc::readdir(entries) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_string_lossy();
        if name.starts_with("runtime.prebind.json.tmp.")
            || name.starts_with("runtime.postbind.json.tmp.")
        {
            found = true;
            break;
        }
    }
    unsafe { libc::closedir(entries) };
    found
}

fn validate_prebind(marker: &PrebindMarker, namespace: &str) -> Result<(), ReadFailure> {
    if marker.schema != 1 || !valid_uuid(&marker.prebind_instance_id) {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if !valid_namespace(&marker.namespace) {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if marker.namespace != namespace || !valid_process_identity(&marker.process_start_identity) {
        return Err(ReadFailure::pre("marker_identity"));
    }
    Ok(())
}

fn validate_postbind(marker: &PostbindMarker, namespace: &str) -> Result<(), ReadFailure> {
    if marker.schema != 1
        || !valid_uuid(&marker.prebind_instance_id)
        || !valid_uuid(&marker.runtime_instance_id)
        || !valid_uuid(&marker.runtime_epoch)
    {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if !valid_namespace(&marker.namespace) {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    if marker.namespace != namespace
        || marker.socket_path != SOCKET_FILE
        || marker.owner_uid != unsafe { libc::geteuid() }
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
    validate_regular_file(&file, unsafe { libc::geteuid() }, &entry_identity)
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

fn inspect_temporary_markers(dir: RawFd) -> Result<(), ReadFailure> {
    let duplicate = unsafe { libc::dup(dir) };
    let scan = fd_to_owned(duplicate).map_err(|_| ReadFailure::pre("marker_identity"))?;
    let raw = scan.into_raw_fd();
    let entries = unsafe { libc::fdopendir(raw) };
    if entries.is_null() {
        unsafe { libc::close(raw) };
        return Err(ReadFailure::pre("marker_identity"));
    }
    let result = scan_temporary_markers(entries);
    unsafe { libc::closedir(entries) };
    result
}

fn scan_temporary_markers(entries: *mut libc::DIR) -> Result<(), ReadFailure> {
    loop {
        errno_reset();
        let entry = unsafe { libc::readdir(entries) };
        if entry.is_null() {
            return if errno() == 0 {
                Ok(())
            } else {
                Err(ReadFailure::pre("marker_identity"))
            };
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let Some(kind) = temporary_kind(&name) else {
            if name == PREBIND_FILE || name == POSTBIND_FILE || name == SOCKET_FILE {
                continue;
            }
            if name.starts_with("runtime.prebind.json")
                || name.starts_with("runtime.postbind.json")
                || name.starts_with(SOCKET_FILE)
            {
                return Err(ReadFailure::pre("marker_invalid"));
            }
            continue;
        };
        let suffix = name
            .rsplit_once(".tmp.")
            .map(|(_, value)| value)
            .unwrap_or_default();
        if !valid_uuid(suffix) {
            return Err(ReadFailure::pre("marker_invalid"));
        }
        let bytes = read_named_file(unsafe { libc::dirfd(entries) }, &name)?;
        let (content_uuid, pid, process_identity) = match kind {
            TempKind::Prebind => match serde_json::from_slice::<PrebindMarker>(&bytes) {
                Ok(value)
                    if value.schema == 1
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
                Err(_) => return Err(ReadFailure::pre("marker_identity")),
            },
            TempKind::Postbind => match serde_json::from_slice::<PostbindMarker>(&bytes) {
                Ok(value)
                    if value.schema == 1
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
                Err(_) => return Err(ReadFailure::pre("marker_identity")),
            },
        };
        if content_uuid != suffix {
            return Err(ReadFailure::pre("marker_invalid"));
        }
        if process_state(pid, &process_identity)? != ProcessState::Dead {
            return Err(ReadFailure::pre("marker_identity"));
        }
    }
}

#[derive(Clone, Copy)]
enum TempKind {
    Prebind,
    Postbind,
}

fn temporary_kind(name: &str) -> Option<TempKind> {
    if name.starts_with("runtime.prebind.json.tmp.") {
        Some(TempKind::Prebind)
    } else if name.starts_with("runtime.postbind.json.tmp.") {
        Some(TempKind::Postbind)
    } else {
        None
    }
}

fn read_named_file(dir: RawFd, name: &str) -> Result<Vec<u8>, ReadFailure> {
    let (file, entry) = open_entry(dir, name).map_err(|_| ReadFailure::pre("marker_invalid"))?;
    validate_regular_file(&file, unsafe { libc::geteuid() }, &entry)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFailure::pre("marker_invalid"))?;
    if bytes.len() as u64 > MARKER_LIMIT {
        return Err(ReadFailure::pre("marker_invalid"));
    }
    let after = fstatat(dir, name).map_err(|_| ReadFailure::pre("marker_invalid"))?;
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
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 || length != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(ReadFailure::post("peer_identity"));
    }
    let credentials = unsafe { credentials.assume_init() };
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
    let base = PathBuf::from(format!("/proc/self/fd/{dir}"));
    if !base.exists() {
        return Err(ReadFailure::pre("anchored_alias_unavailable"));
    }
    Ok(base.join(child))
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
        if error.raw_os_error() == Some(libc::ENOENT) {
            EntryError::Missing
        } else {
            EntryError::Invalid
        }
    })?;
    if before.st_nlink != 1 || !regular_stat(&before) {
        return Err(EntryError::Invalid);
    }
    let c_name = CString::new(name).map_err(|_| EntryError::Invalid)?;
    let fd = unsafe {
        libc::openat(
            dir,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(EntryError::Invalid);
    }
    let file = unsafe { File::from_raw_fd(fd) };
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

fn identity(stat: &libc::stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn fstat(fd: RawFd) -> Result<libc::stat, TrustedPathError> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(TrustedPathError::Invalid);
    }
    Ok(unsafe { stat.assume_init() })
}

fn fstatat(dir: RawFd, name: &str) -> std::io::Result<libc::stat> {
    let name =
        CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

fn regular_stat(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
}

fn directory_stat(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn socket_stat(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

fn socket_mode_allowed(stat: &libc::stat, euid: u32) -> bool {
    stat.st_uid == euid && stat.st_mode & 0o777 == 0o600
}

fn valid_process_identity(value: &str) -> bool {
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
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let value = value.trim().to_string();
    valid_uuid(&value).then_some(value)
}

fn process_start(pid: u32) -> Result<Option<String>, ReadFailure> {
    let path = format!("/proc/{pid}/stat");
    let stat = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ReadFailure::pre("marker_identity")),
    };
    let close = stat
        .rfind(')')
        .ok_or_else(|| ReadFailure::pre("marker_identity"))?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields
        .get(19)
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|value| (*value).to_string())
        .map(Some)
        .ok_or_else(|| ReadFailure::pre("marker_identity"))
}

fn errno_reset() {
    unsafe { *libc::__errno_location() = 0 }
}

fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel serves `system.posix_acl_access` in text form, so the parser
    /// is exercised on the spelling the kernel produces. Testing the parser
    /// rather than the syscall keeps this deterministic on a filesystem with
    /// no POSIX ACL support, where the syscall itself cannot be exercised.
    #[test]
    fn a_posix_acl_is_read_in_the_spelling_the_kernel_writes() {
        for admitted in [
            "u::rw-,g::r--,o::r--",
            "u::rwx,g::r-x,o::r--,m::rwx",
            "u::rw-,g::r--,o::r--,u:1002:r-x",
        ] {
            assert_eq!(
                validate_acl_value(admitted.as_bytes()),
                Ok(()),
                "{admitted} grants nobody a write the mode does not already show"
            );
        }
        for refused in [
            "u::rw-,g::r--,o::r--,u:1002:rwx",
            "u::rw-,g::r--,o::r--,g:1002:-w-",
            "u::rw-x,zz:1002:rwx",
            "u::rw-,g::r--,o::q--",
            "u::",
            "u::rw-,g::r--,o::r--,u:1002:rwx:extra",
            "u::rw-,g::r--,o::r--,u:notanid:r--",
        ] {
            assert_eq!(
                validate_acl_value(refused.as_bytes()),
                Err(TrustedPathError::Invalid),
                "{refused} is a named write or a value this code cannot read"
            );
        }
    }

    #[test]
    fn process_identity_grammar_is_ascii_and_structured() {
        let boot = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert!(valid_process_identity(&format!("linux:v1:{boot}:123")));
        assert!(!valid_process_identity(&format!("linux:v1:{boot}:123 ")));
        assert!(!valid_process_identity(&format!("linux:v2:{boot}:123")));
    }

    /// A fresh home has no `~/.local` to hold the app directory, so the walk
    /// ends on an absent intermediate component. That is an absence, not a
    /// refusal, and it is what lets the caller run the command locally. A
    /// component that exists but is untrustworthy still refuses.
    #[test]
    fn an_absent_ancestor_is_an_absence_and_an_untrustworthy_one_is_not() {
        use std::os::unix::fs::PermissionsExt;

        let euid = unsafe { libc::geteuid() };
        let base = tempfile::tempdir().expect("temp base");
        let absent = base.path().join("never-created").join("app-dir");
        assert!(matches!(
            open_trusted_directory(&absent, euid),
            Err(TrustedPathError::Missing)
        ));

        let present = base.path().join("app-dir");
        std::fs::create_dir(&present).expect("create");
        assert!(open_trusted_directory(&present, euid).is_ok());

        // Sticky and world-writable is what /tmp looks like, but the allowance
        // is for a root-owned ancestor only, so this one still refuses.
        let world = base.path().join("world");
        std::fs::create_dir(&world).expect("create");
        let mut permissions = std::fs::metadata(&world).expect("stat").permissions();
        permissions.set_mode(0o777 | libc::S_ISVTX);
        std::fs::set_permissions(&world, permissions).expect("chmod");
        let world_child = world.join("app-dir");
        std::fs::create_dir(&world_child).expect("create");
        assert!(matches!(
            open_trusted_directory(&world_child, euid),
            Err(TrustedPathError::Refused(_))
        ));
    }

    /// A home reached through a symlink is not a tampered home, and macOS
    /// `/tmp` is a symlink on every machine, so a prefix component is followed
    /// and the directory it resolves to is verified by descriptor. What must
    /// not change is the verification: a symlinked prefix whose target is
    /// world-writable is still refused, and a symlinked *final* component is
    /// still a refusal, because that would swap the app directory itself.
    #[test]
    fn a_symlinked_prefix_is_followed_but_every_check_still_applies() {
        use std::os::unix::fs::PermissionsExt;

        let euid = unsafe { libc::geteuid() };
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

    /// A refused path has to say which directory it refused, what is wrong
    /// with it and what clears it. The refusal itself is unchanged: same
    /// code, same exit, still no admission: but `daemon read: marker_invalid`
    /// names nothing an operator can act on, and the walk already knows the
    /// component, its mode and the rule it broke.
    #[test]
    fn a_refused_component_names_its_path_its_mode_and_its_remedy() {
        let euid = unsafe { libc::geteuid() };
        let base = tempfile::tempdir().expect("temp base");
        let app = base.path().join("app-dir");
        std::fs::create_dir(&app).expect("create");
        let mut permissions = std::fs::metadata(&app).expect("stat").permissions();
        permissions.set_mode(0o770);
        std::fs::set_permissions(&app, permissions).expect("chmod");

        let failure =
            trusted_path_failure(open_trusted_directory(&app, euid).expect_err("0770 refuses"));
        assert_eq!(
            failure.code(),
            "marker_invalid",
            "the refusal keeps its code; a diagnosis is not a different answer"
        );
        let outcome = super::super::ReadOutcome::from(failure);
        let stderr = outcome.stderr.expect("a refusal always says something");
        assert!(
            stderr.contains(&app.display().to_string()),
            "the refusal must name the path the operator has to fix: {stderr}"
        );
        assert!(
            stderr.contains("0770"),
            "the refusal must name the mode it found: {stderr}"
        );
        assert!(
            stderr.contains(&format!("chmod go-w '{}'", app.display())),
            "the refusal must name the command that clears this cause: {stderr}"
        );
        assert_eq!(outcome.exit, 2, "the refusal keeps its exit");
    }

    /// `/tmp` is world-writable and sticky, and the walk must still pass
    /// through it: a test namespace and a crash artifact both live there. A
    /// sticky directory that is not root-owned, and the final component in any
    /// case, stay refused.
    #[test]
    fn a_sticky_root_ancestor_is_walkable_and_nothing_else_is() {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = libc::S_IFDIR | 0o777;

        stat.st_uid = 0;
        assert!(!group_other_writes_allowed(&stat, true));
        stat.st_mode |= libc::S_ISVTX;
        assert!(group_other_writes_allowed(&stat, false));
        assert!(!group_other_writes_allowed(&stat, true));

        stat.st_uid = unsafe { libc::geteuid() };
        assert!(!group_other_writes_allowed(&stat, false));

        stat.st_uid = 0;
        stat.st_mode = libc::S_IFDIR | 0o755;
        assert!(group_other_writes_allowed(&stat, false));
        stat.st_mode = libc::S_IFDIR | 0o757;
        assert!(!group_other_writes_allowed(&stat, false));
    }

    #[test]
    fn temporary_name_requires_canonical_uuid() {
        let name = "runtime.prebind.json.tmp.aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert!(temporary_kind(name).is_some());
        assert!(valid_uuid(name.rsplit_once(".tmp.").unwrap().1));
        assert!(!valid_uuid("AAAAAAAA-bbbb-cccc-dddd-eeeeeeeeeeee"));
    }

    /// A crash between the publisher's exclusive create and its rename leaves a
    /// temporary marker whose body does not parse. The client refuses that file
    /// until the publisher reaps it, and accepts the directory the reap leaves
    /// behind: the two halves agree on which directory is trustworthy.
    #[test]
    fn a_directory_whose_temporary_was_reaped_is_admitted() {
        let dir = tempfile::tempdir().expect("namespace");
        let torn = dir
            .path()
            .join("runtime.prebind.json.tmp.aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        std::fs::write(&torn, b"").expect("torn temporary");
        let namespace = File::open(dir.path()).expect("open namespace");

        assert!(
            inspect_temporary_markers(namespace.as_raw_fd()).is_err(),
            "an unparsable temporary is not admitted"
        );

        std::fs::remove_file(&torn).expect("the publisher reaps it");
        inspect_temporary_markers(namespace.as_raw_fd()).expect("the reaped directory is admitted");
    }

    /// A namespace whose markers are all present, all consistent and all
    /// perfectly valid, naming a pid that is provably not running: that is an
    /// absence, and it must answer as one. Reporting it as an identity fault
    /// would send the read back through the retry loop and spend the whole
    /// establishment budget before saying the same thing twice.
    #[tokio::test]
    async fn a_dead_publisher_is_an_absence_rather_than_a_retryable_identity() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("namespace");
        let lock = dir.path().join(LOCK_FILE);
        std::fs::write(&lock, b"").expect("lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        // A pid no process can hold, recorded against this boot, so the walk
        // proves it dead rather than failing to prove anything.
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
            "owner_uid": unsafe { libc::geteuid() },
            "socket_device": 1,
            "socket_inode": 1,
            "socket_creator_pid": dead_pid,
        });
        // The admission requires exactly 0600 on both markers, so the mode the
        // umask would give a plain write is corrected here rather than tested
        // as a separate refusal.
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
            Instant::now() + crate::cli::runtime_read::ESTABLISHMENT_BUDGET,
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

    /// A daemon killed between the prebind write and the postbind write leaves
    /// the pair half-published, and only a daemon that starts again reaps that
    /// shape. The prebind alone already carries the pid and its start identity,
    /// so the same proof that makes a complete dead pair an absence applies
    /// here: the read must be handed to the local store rather than retried
    /// until the establishment budget runs out.
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
        // The socket the publisher binds before it writes the postbind.
        std::fs::write(dir.path().join(SOCKET_FILE), b"").expect("socket");

        let opened = std::fs::File::open(dir.path()).expect("open namespace");
        let namespace = OwnedNamespace {
            name: namespace_name.to_string(),
            home: dir.path().to_path_buf(),
            dir: OwnedFd::from(opened),
        };
        let Err(error) = connect_admission(
            namespace,
            Instant::now() + crate::cli::runtime_read::ESTABLISHMENT_BUDGET,
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
    fn os_path_helpers_do_not_create_directories() {
        let base = std::env::temp_dir().join(format!("aoe-read-{}", uuid::Uuid::new_v4()));
        let path = base.join("missing");
        assert!(!path.exists());
        assert!(app_path_and_home().is_ok());
        assert!(!path.exists());
    }
    #[test]
    fn connected_socket_identity_does_not_require_path_inode_equality() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = tempfile::tempdir().expect("temp namespace");
        let socket_path = dir.path().join(SOCKET_FILE);
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let client = UnixStream::connect(&socket_path).expect("connect socket");
        let (server, _) = listener.accept().expect("accept socket");
        let dir_file = File::open(dir.path()).expect("open namespace");
        let path_stat = fstatat(dir_file.as_raw_fd(), SOCKET_FILE).expect("stat socket");
        let marker = PostbindMarker {
            schema: 1,
            pid: std::process::id(),
            process_start_identity: String::new(),
            prebind_instance_id: String::new(),
            runtime_instance_id: String::new(),
            runtime_epoch: String::new(),
            namespace: String::new(),
            socket_path: SOCKET_FILE.into(),
            owner_uid: unsafe { libc::geteuid() },
            socket_device: path_stat.st_dev as u64,
            socket_inode: path_stat.st_ino as u64,
            socket_creator_pid: std::process::id(),
        };

        assert!(validate_connected_socket(
            server.as_raw_fd(),
            dir_file.as_raw_fd(),
            &marker,
            unsafe { libc::geteuid() },
        )
        .is_ok());
        drop(client);
    }
}

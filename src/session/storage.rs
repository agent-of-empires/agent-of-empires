//! Session storage - JSON file persistence with in-process and cross-process locking.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;

use super::{
    get_app_dir, get_profile_dir, get_profile_dir_path, resolve_existing_profile, Group, Instance,
};

/// Sidecar lock file name for per-profile storage.
pub(crate) const STORAGE_LOCK_FILENAME: &str = ".storage.lock";

/// Sidecar lock file name for the global workspace-ordering file.
const WORKSPACE_LOCK_FILENAME: &str = ".workspace-ordering.lock";
/// Sidecar lock prefix for one session's launch lifecycle.
const INSTANCE_LIFECYCLE_LOCK_PREFIX: &str = ".instance-lifecycle-";
/// Sidecar lock for every mutation that can create or change a session's `(title,
/// project_path)` identity.
const SESSION_IDENTITY_LOCK_FILENAME: &str = ".title-mutation.lock";
/// Sidecar lock prefix for one session's title persistence plus tmux rekey.
const SESSION_TITLE_LOCK_PREFIX: &str = ".session-title-";

/// Emit a tracing warn if the cross-process `flock` is held by a peer for longer than this.
const FLOCK_WAIT_WARN_AFTER: Duration = Duration::from_secs(1);

/// Write `content` atomically (temp file + data/metadata fsync + rename + best-effort dir
/// fsync).
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let resolved = resolve_symlink_chain(path)?;
    atomic_write_resolved(&resolved, content)
}

fn atomic_write_resolved(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "atomic_write needs a path with a parent: {}",
            path.display()
        )
    })?;
    let existing_perms = fs::metadata(path).ok().map(|m| m.permissions());
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    if let Some(perms) = existing_perms {
        tmp.as_file().set_permissions(perms)?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    // Best-effort dir fsync so the rename itself survives power loss.
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Replace `root`/`rel` with `content`, creating `rel`'s directories, without ever
/// traversing a symlink below `root`.
#[cfg(unix)]
pub(crate) fn replace_file_no_follow(root: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, renameat, OFlag};
    use nix::sys::stat::{fchmod, mkdirat, Mode};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::os::fd::OwnedFd;

    let (dirs, file_name) = split_no_follow_rel(rel, "replace_file_no_follow")?;

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    // The anchor is created the ordinary way; anything that could plant a
    // symlink above it already owns the tree AoE writes into.
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut dir: OwnedFd = open(root, dir_flags, Mode::empty())
        .with_context(|| format!("opening {}", root.display()))?;
    for name in dirs {
        match mkdirat(&dir, name, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => {
                return Err(anyhow!(e)).with_context(|| {
                    format!("creating {} under {}", rel.display(), root.display())
                })
            }
        }
        dir = openat(&dir, name, dir_flags | OFlag::O_NOFOLLOW, Mode::empty()).with_context(
            || {
                format!(
                    "opening {:?} under {}: a symlink there would redirect the write",
                    name,
                    root.display()
                )
            },
        )?;
    }

    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        NO_FOLLOW_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let tmp = openat(
        &dir,
        tmp_name.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .with_context(|| format!("creating a temp file under {}", root.display()))?;

    let written = (|| -> Result<()> {
        let mut file = fs::File::from(tmp);
        file.write_all(content)?;
        fchmod(&file, Mode::from_bits_truncate(0o644))?;
        file.sync_all()?;
        drop(file);
        renameat(&dir, tmp_name.as_str(), &dir, file_name)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = unlinkat(&dir, tmp_name.as_str(), UnlinkatFlags::NoRemoveDir);
    }
    written.with_context(|| format!("writing {} under {}", rel.display(), root.display()))
}

/// Split `rel` into the directories to walk and the final name, rejecting anything but
/// plain relative components so no `..` or absolute segment can leave the anchor.
fn split_no_follow_rel<'a>(
    rel: &'a Path,
    what: &str,
) -> Result<(Vec<&'a std::ffi::OsStr>, &'a std::ffi::OsStr)> {
    let mut dirs = Vec::new();
    let mut file_name = None;
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!(
                "{what} needs a plain relative path, got {}",
                rel.display()
            ));
        };
        if components.peek().is_some() {
            dirs.push(name);
        } else {
            file_name = Some(name);
        }
    }
    let file_name = file_name.ok_or_else(|| anyhow!("{what} needs a file name"))?;
    Ok((dirs, file_name))
}

/// Read `root`/`rel` as UTF-8 without ever traversing a symlink below `root`.
#[cfg(unix)]
pub(crate) fn read_file_no_follow(root: &Path, rel: &Path) -> Result<Option<String>> {
    use nix::fcntl::{open, openat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, Mode};
    use std::io::Read;

    let (dirs, file_name) = split_no_follow_rel(rel, "read_file_no_follow")?;

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let Ok(mut dir) = open(root, dir_flags, Mode::empty()) else {
        return Ok(None);
    };
    for name in dirs {
        let Ok(next) = openat(&dir, name, dir_flags | OFlag::O_NOFOLLOW, Mode::empty()) else {
            return Ok(None);
        };
        dir = next;
    }

    let regular = |mode| mode & libc::S_IFMT == libc::S_IFREG;

    // Stat the name on the descriptor first.
    let Ok(before) = fstatat(&dir, file_name, AtFlags::AT_SYMLINK_NOFOLLOW) else {
        return Ok(None);
    };
    if !regular(before.st_mode) {
        return Ok(None);
    }

    let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
    let Ok(fd) = openat(&dir, file_name, flags, Mode::empty()) else {
        return Ok(None);
    };
    let mut file = fs::File::from(fd);
    let Ok(after) = fstat(&file) else {
        return Ok(None);
    };
    if !regular(after.st_mode) || after.st_dev != before.st_dev || after.st_ino != before.st_ino {
        return Ok(None);
    }
    let mut content = String::new();
    Ok(file.read_to_string(&mut content).ok().map(|_| content))
}

/// Serial for the per-attempt temp name in [`replace_file_no_follow`], so two
/// writers in one process cannot pick the same one inside a clock tick.
#[cfg(unix)]
static NO_FOLLOW_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Windows keeps the check-then-open the unix arm closes; the sandbox this guards against
/// is Linux and macOS only.
#[cfg(not(unix))]
pub(crate) fn read_file_no_follow(root: &Path, rel: &Path) -> Result<Option<String>> {
    split_no_follow_rel(rel, "read_file_no_follow")?;
    let path = root.join(rel);
    Ok(fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.is_file())
        .then(|| fs::read_to_string(&path).ok())
        .flatten())
}

/// Windows has no `O_NOFOLLOW` walk here; the sandbox this guards against is
/// Linux and macOS only. Kept so the module compiles.
#[cfg(not(unix))]
pub(crate) fn replace_file_no_follow(root: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    let path = root.join(rel);
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("replace_file_no_follow needs a path with a parent"))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)?;
    Ok(())
}

/// Resolve `path` through a symlink chain to the underlying target file.
pub(crate) fn resolve_symlink_chain(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    let mut hops: usize = 0;
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect {}", current.display()));
            }
            Ok(_) => {}
        }

        if hops >= 32 {
            return Err(anyhow!("Symlink chain too deep: {}", path.display()));
        }

        let target = fs::read_link(&current)
            .with_context(|| format!("Failed to read symlink {}", current.display()))?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
        hops += 1;
    }
}

/// Serialized read-modify-write of a small standalone data file.
pub(crate) fn locked_update<T, R, E>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T>,
    serialize: impl FnOnce(&T) -> Result<String>,
    mutate: impl FnOnce(&mut T) -> std::result::Result<R, E>,
) -> Result<std::result::Result<R, E>>
where
    T: Default,
{
    let path = &resolve_symlink_chain(path)?;
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "locked_update needs a path with a parent: {}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("locked_update needs a file path: {}", path.display()))?;
    let lock_name = format!(".{}.lock", file_name.to_string_lossy());
    let _flock = acquire_storage_flock(dir, &lock_name)?;

    let mut value = match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => T::default(),
        Ok(content) => parse(&content)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let result = mutate(&mut value);
    if result.is_ok() {
        atomic_write(path, serialize(&value)?.as_bytes())?;
        // Best-effort here, unlike atomic_write's "every fallible mutation before the
        // rename" contract.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    Ok(result)
}

/// Process-wide registry of per-profile save mutexes.
fn save_lock_for(profile: &str) -> Arc<Mutex<()>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .entry(profile.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Dedicated lock for the global `workspace-ordering.json` file.
fn workspace_ordering_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard for a held cross-process `flock`.
pub(crate) struct StorageFlock {
    file: fs::File,
}

impl Drop for StorageFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn app_dir_for_profile_dir(profile_dir: &Path) -> &Path {
    profile_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
        .and_then(Path::parent)
        .unwrap_or(profile_dir)
}

fn acquire_transition_flocks_for_profile_dirs(profile_dirs: &[&Path]) -> Result<Vec<StorageFlock>> {
    let mut app_dirs: Vec<PathBuf> = profile_dirs
        .iter()
        .map(|dir| app_dir_for_profile_dir(dir).to_path_buf())
        .collect();
    app_dirs.sort();
    app_dirs.dedup();
    app_dirs
        .iter()
        .map(|dir| {
            acquire_storage_shared_flock(dir, crate::migrations::v027_isolate_sandbox_stores::LOCK)
        })
        .collect()
}

#[cfg(unix)]
fn same_filesystem_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_filesystem_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    // Portable metadata exposes no stable file identity.
    false
}

fn paths_share_filesystem_identity(left: &Path, right: &Path) -> Result<bool> {
    Ok(same_filesystem_identity(
        &fs::metadata(left)?,
        &fs::metadata(right)?,
    ))
}

fn existing_paths_share_filesystem_identity(left: &Path, right: &Path) -> Result<bool> {
    let left = resolve_symlink_chain(left)?;
    let right = resolve_symlink_chain(right)?;
    match (fs::metadata(&left), fs::metadata(&right)) {
        (Ok(left), Ok(right)) => Ok(same_filesystem_identity(&left, &right)),
        (Err(error), _) | (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(false)
        }
        (Err(error), _) | (_, Err(error)) => Err(error.into()),
    }
}

fn open_storage_lock_file(dir: &Path, name: &str) -> Result<(fs::File, PathBuf)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    Ok((file, path))
}

#[cfg(test)]
thread_local! {
    static LOCK_CONTENTION_OBSERVER: std::cell::RefCell<Option<std::sync::mpsc::Sender<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn observe_lock_contention_for_test(
    sender: std::sync::mpsc::Sender<PathBuf>,
) -> impl Drop {
    struct Observer(std::marker::PhantomData<std::rc::Rc<()>>);
    impl Drop for Observer {
        fn drop(&mut self) {
            LOCK_CONTENTION_OBSERVER.with(|slot| slot.borrow_mut().take());
        }
    }
    LOCK_CONTENTION_OBSERVER.with(|slot| {
        assert!(slot.borrow_mut().replace(sender).is_none());
    });
    Observer(std::marker::PhantomData)
}

#[cfg(test)]
fn report_lock_contention_for_test(path: &Path) {
    LOCK_CONTENTION_OBSERVER.with(|slot| {
        if let Some(sender) = slot.borrow_mut().take() {
            let _ = sender.send(path.to_path_buf());
        }
    });
}

fn acquire_open_storage_flock(file: fs::File, path: &Path) -> Result<StorageFlock> {
    if let Err(e) = file.try_lock_exclusive() {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(e.into());
        }
        #[cfg(feature = "test-support")]
        if let Some(marker) = std::env::var_os("AOE_E2E_STORAGE_LOCK_CONTENDED") {
            fs::write(marker, path.as_os_str().as_encoded_bytes())?;
        }
        #[cfg(test)]
        report_lock_contention_for_test(path);
        let started = Instant::now();
        let mut warned = false;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let waited = started.elapsed();
                    if waited >= FLOCK_WAIT_WARN_AFTER {
                        if warned {
                            tracing::info!(
                                target: "session.store",
                                ?waited,
                                path = %path.display(),
                                "storage flock acquired after wait"
                            );
                        } else {
                            tracing::warn!(
                                target: "session.store",
                                ?waited,
                                path = %path.display(),
                                "storage flock contended for >1s; another aoe process held it"
                            );
                        }
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !warned && started.elapsed() >= FLOCK_WAIT_WARN_AFTER {
                        tracing::warn!(
                            target: "session.store",
                            path = %path.display(),
                            "storage flock contended for >1s; another aoe process is mid-write"
                        );
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(StorageFlock { file })
}
fn acquire_open_storage_shared_flock(file: fs::File, path: &Path) -> Result<StorageFlock> {
    if let Err(e) = FileExt::try_lock_shared(&file) {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(e.into());
        }
        #[cfg(test)]
        report_lock_contention_for_test(path);
        let started = Instant::now();
        let mut warned = false;
        loop {
            match FileExt::try_lock_shared(&file) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !warned && started.elapsed() >= FLOCK_WAIT_WARN_AFTER {
                        tracing::warn!(
                            target: "session.store",
                            path = %path.display(),
                            "storage transition flock contended for >1s; migration is active"
                        );
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(StorageFlock { file })
}

/// Acquire the app-wide session identity-mutation lock.
pub(crate) fn acquire_session_identity_lock() -> Result<StorageFlock> {
    acquire_storage_flock(&get_app_dir()?, SESSION_IDENTITY_LOCK_FILENAME)
}

/// Serialize one session's title commit and post-commit tmux rekey across profiles and
/// processes.
pub(crate) fn acquire_session_title_lock(instance_id: &str) -> Result<StorageFlock> {
    super::validate_instance_id(instance_id)
        .context("refusing session title lock for invalid instance id")?;
    acquire_storage_flock(
        &get_app_dir()?,
        &format!("{SESSION_TITLE_LOCK_PREFIX}{instance_id}.lock"),
    )
}

// Test-only crash injection for the profile-move transaction.
#[cfg(test)]
thread_local! {
    static TEST_CRASH_POINTS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn arm_test_crash_point(name: &str) {
    TEST_CRASH_POINTS.with(|points| points.borrow_mut().push(name.to_string()));
}

#[cfg(test)]
fn test_crash_point(name: &str) {
    let armed = TEST_CRASH_POINTS.with(|points| points.borrow().iter().any(|armed| armed == name));
    if armed {
        panic!("simulated crash at crash point `{name}`");
    }
}

#[cfg(test)]
pub(crate) fn disarm_test_crash_points() {
    TEST_CRASH_POINTS.with(|points| points.borrow_mut().clear());
}

/// RAII wrapper so a failing assertion between arm and disarm can never leave
/// an armed point panicking an unrelated test scheduled on the same thread.
#[cfg(test)]
pub(crate) struct ArmedCrashPoint;

#[cfg(test)]
impl ArmedCrashPoint {
    pub(crate) fn arm(name: &'static str) -> Self {
        arm_test_crash_point(name);
        Self
    }
}

#[cfg(test)]
impl Drop for ArmedCrashPoint {
    fn drop(&mut self) {
        disarm_test_crash_points();
    }
}

pub(crate) fn sync_parent_directory(path: &Path) -> Result<()> {
    let resolved = resolve_symlink_chain(path)?;
    sync_resolved_parent_directory(&resolved)
}

#[cfg(unix)]
fn sync_resolved_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::File::open(parent)
        .with_context(|| format!("opening profile directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing profile directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_resolved_parent_directory(path: &Path) -> Result<()> {
    path.parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    // Rust exposes no portable directory flush outside Unix.
    Ok(())
}

pub(crate) fn atomic_write_verified(path: &Path, content: &[u8]) -> Result<()> {
    atomic_write_verified_resolved(path, content).map(|_| ())
}

fn restore_file_durably<W, S>(
    path: &Path,
    content: &[u8],
    write_context: W,
    sync_context: S,
) -> Result<()>
where
    W: FnOnce() -> String,
    S: FnOnce() -> String,
{
    atomic_write_verified(path, content).with_context(write_context)?;
    sync_parent_directory(path).with_context(sync_context)
}

fn atomic_write_verified_resolved(path: &Path, content: &[u8]) -> Result<PathBuf> {
    let resolved = resolve_symlink_chain(path)?;
    if let Err(error) = atomic_write_resolved(&resolved, content) {
        if fs::read(&resolved).is_ok_and(|persisted| persisted == content) {
            tracing::warn!(
                target: "session.store",
                error = %error,
                path = %resolved.display(),
                "profile move write committed but reported an error"
            );
            return Ok(resolved);
        }
        return Err(error);
    }
    Ok(resolved)
}

/// Acquire the cross-process advisory `flock` on `<dir>/<name>` by polling
/// `try_lock_exclusive` every 50ms until it is granted.
pub(crate) fn acquire_storage_flock(dir: &Path, name: &str) -> Result<StorageFlock> {
    let (file, path) = open_storage_lock_file(dir, name)?;
    acquire_open_storage_flock(file, &path)
}

pub(crate) fn acquire_storage_shared_flock(dir: &Path, name: &str) -> Result<StorageFlock> {
    let (file, path) = open_storage_lock_file(dir, name)?;
    acquire_open_storage_shared_flock(file, &path)
}

/// [`acquire_storage_flock`] without the wait.
pub(crate) fn try_acquire_storage_flock(dir: &Path, name: &str) -> Result<Option<StorageFlock>> {
    let (file, _path) = open_storage_lock_file(dir, name)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(StorageFlock { file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub struct Storage {
    profile: String,
    sessions_path: PathBuf,
    save_lock: Arc<Mutex<()>>,
    file_watch: Arc<FileWatchService>,
    #[cfg(test)]
    fail_writes_for_test: bool,
}

// Cross-device-syncable sidebar ordering.
#[derive(serde::Deserialize, serde::Serialize, Default)]
pub struct WorkspaceOrdering {
    pub order: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct GroupMovePlan {
    source_path: String,
    target_path: String,
    move_subtree: bool,
}

impl GroupMovePlan {
    pub(crate) fn single(source_path: &str, target_path: &str) -> Self {
        Self {
            source_path: source_path.to_string(),
            target_path: target_path.to_string(),
            move_subtree: false,
        }
    }

    pub(crate) fn subtree(source_path: &str, target_path: &str) -> Self {
        Self {
            source_path: source_path.to_string(),
            target_path: target_path.to_string(),
            move_subtree: true,
        }
    }
}

struct MoveTransactionPlan<'a> {
    group_move: &'a GroupMovePlan,
    merge_complete_post: bool,
    /// A tool change on this move swaps accounts of one agent rather than
    /// agents, so the moved row keeps its conversation; see
    /// [`Instance::merge_profile_move_diff`].
    account_swap: bool,
}

fn apply_group_move(
    plan: &GroupMovePlan,
    source_instances: &[Instance],
    source_groups: &mut Vec<Group>,
    target_instances: &[Instance],
    target_groups: &mut Vec<Group>,
) {
    let source_prefix = format!("{}/", plan.source_path);
    let matches_source = |path: &str| {
        !plan.source_path.is_empty()
            && (path == plan.source_path || (plan.move_subtree && path.starts_with(&source_prefix)))
    };
    let transfer_source_metadata = plan.move_subtree || plan.source_path == plan.target_path;
    let moving_groups: Vec<Group> = source_groups
        .iter()
        .filter(|group| transfer_source_metadata && matches_source(&group.path))
        .cloned()
        .collect();

    if !plan.target_path.is_empty() {
        for mut group in moving_groups {
            let path = if group.path == plan.source_path {
                plan.target_path.clone()
            } else {
                format!(
                    "{}{}",
                    plan.target_path,
                    &group.path[plan.source_path.len()..]
                )
            };
            if target_groups.iter().any(|existing| existing.path == path) {
                continue;
            }
            group.name = path.rsplit('/').next().unwrap_or(&path).to_string();
            group.path = path;
            group.children.clear();
            target_groups.push(group);
        }
    }

    if plan.move_subtree {
        source_groups.retain(|group| !matches_source(&group.path));
    } else if !plan.source_path.is_empty() {
        let source_still_uses_path = source_instances.iter().any(|instance| {
            instance.group_path == plan.source_path
                || instance.group_path.starts_with(&source_prefix)
        });
        let has_explicit_descendant = source_groups
            .iter()
            .any(|group| group.path.starts_with(&source_prefix));
        if !source_still_uses_path && !has_explicit_descendant {
            source_groups.retain(|group| group.path != plan.source_path);
        }
    }

    // Re-tree both sides so a group implied only by a moved instance's path materialises as
    // an explicit row.
    *source_groups =
        super::GroupTree::new_with_groups(source_instances, source_groups).get_all_groups();
    *target_groups =
        super::GroupTree::new_with_groups(target_instances, target_groups).get_all_groups();
}

impl Storage {
    pub fn new(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = if profile.is_empty() {
            super::config::resolve_default_profile()
        } else {
            profile.to_string()
        };

        let profile_dir = get_profile_dir(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }

    /// Construct a `Storage` wired to a noop `FileWatchService`.
    pub fn new_unwatched(profile: &str) -> Result<Self> {
        Self::new(profile, FileWatchService::noop())
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_path(profile: &str, sessions_path: PathBuf) -> Self {
        Self {
            profile: profile.to_string(),
            sessions_path,
            save_lock: save_lock_for(profile),
            file_watch: FileWatchService::noop(),
            fail_writes_for_test: false,
        }
    }

    /// Construct a `Storage` for an existing profile, never creating it.
    pub fn open(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = resolve_existing_profile(profile)?;
        let profile_dir = get_profile_dir_path(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }

    /// [`Storage::open`] wired to a noop `FileWatchService`.
    pub fn open_unwatched(profile: &str) -> Result<Self> {
        Self::open(profile, FileWatchService::noop())
    }

    /// Serialize launch/restart and explicit resume-target mutation for one instance across
    /// every process using this profile.
    pub(crate) fn acquire_instance_lifecycle_lock(
        &self,
        instance_id: &str,
    ) -> Result<StorageFlock> {
        super::validate_instance_id(instance_id)
            .context("refusing lifecycle lock for invalid instance id")?;
        let profile_dir = self
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no profile directory"))?;
        acquire_storage_flock(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )
    }

    #[cfg(test)]
    pub(crate) fn set_fail_writes_for_test(&mut self, fail: bool) {
        self.fail_writes_for_test = fail;
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Absolute path of this profile's `sessions.json`. Recovery and the
    /// duplicate-detection surface report it so users can act on exact files.
    pub(crate) fn sessions_path(&self) -> &Path {
        &self.sessions_path
    }

    pub fn load(&self) -> Result<Vec<Instance>> {
        if !self.sessions_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&self.sessions_path)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }

        let rows: Vec<serde_json::Value> = serde_json::from_str(&content)?;
        let mut instances = Vec::with_capacity(rows.len());
        let mut corrupt: Vec<serde_json::Value> = Vec::new();
        for (idx, row) in rows.into_iter().enumerate() {
            match <Instance as serde::Deserialize>::deserialize(&row) {
                Ok(mut inst) => {
                    inst.set_file_watch(self.file_watch.clone());
                    instances.push(inst);
                }
                Err(e) => {
                    tracing::warn!(
                        profile = %self.profile,
                        row = idx,
                        error = %e,
                        path = %self.sessions_path.display(),
                        "skipping corrupt session row"
                    );
                    corrupt.push(row);
                }
            }
        }

        if !corrupt.is_empty() {
            self.quarantine_corrupt_rows(&corrupt);
        }

        Ok(instances)
    }

    fn quarantine_corrupt_rows(&self, rows: &[serde_json::Value]) {
        let path = self.sessions_path.with_file_name("sessions.corrupt.jsonl");
        Self::write_corrupt_rows_quarantine(&path, rows, "session");
    }

    fn quarantine_corrupt_group_rows(&self, rows: &[serde_json::Value]) {
        let path = self.sessions_path.with_file_name("groups.corrupt.jsonl");
        Self::write_corrupt_rows_quarantine(&path, rows, "group");
    }

    /// Write corrupt rows to a sibling quarantine sidecar for later inspection and manual
    /// recovery.
    fn write_corrupt_rows_quarantine(path: &Path, rows: &[serde_json::Value], row_kind: &str) {
        let mut buf = String::new();
        for row in rows {
            match serde_json::to_string(row) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    row_kind = %row_kind,
                    "failed to serialise corrupt row for quarantine"
                ),
            }
        }
        if buf.is_empty() {
            return;
        }

        // `atomic_write` (not `fs::write`) so the sidecar matches the durability and
        // privacy guarantees of the source JSON file.
        if let Err(e) = atomic_write(path, buf.as_bytes()) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                row_kind = %row_kind,
                "failed to write quarantine file"
            );
        }
    }

    pub fn load_with_groups(&self) -> Result<(Vec<Instance>, Vec<Group>)> {
        let instances = self.load()?;

        let groups_path = self.sessions_path.with_file_name("groups.json");
        let groups = if groups_path.exists() {
            let content = fs::read_to_string(&groups_path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                let rows: Vec<serde_json::Value> = serde_json::from_str(&content)?;
                let mut groups = Vec::with_capacity(rows.len());
                let mut corrupt: Vec<serde_json::Value> = Vec::new();
                for (idx, row) in rows.into_iter().enumerate() {
                    match <Group as serde::Deserialize>::deserialize(&row) {
                        Ok(group) => groups.push(group),
                        Err(e) => {
                            tracing::warn!(
                                profile = %self.profile,
                                row = idx,
                                error = %e,
                                path = %groups_path.display(),
                                "skipping corrupt group row"
                            );
                            corrupt.push(row);
                        }
                    }
                }

                if !corrupt.is_empty() {
                    self.quarantine_corrupt_group_rows(&corrupt);
                }

                groups
            }
        } else {
            Vec::new()
        };

        Ok((instances, groups))
    }

    /// Locked load -> mutate -> save.
    pub fn update<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        #[cfg(test)]
        let _mu = crate::session::test_support::lock_reporting_contention(&self.save_lock, || {
            report_lock_contention_for_test(&self.sessions_path)
        })
        .unwrap_or_else(|error| error.into_inner());
        #[cfg(not(test))]
        let _mu = self
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir = self.sessions_path.parent().ok_or_else(|| {
            anyhow!(
                "sessions_path missing parent: {}",
                self.sessions_path.display()
            )
        })?;
        let _transition = acquire_storage_shared_flock(
            app_dir_for_profile_dir(profile_dir),
            crate::migrations::v027_isolate_sandbox_stores::LOCK,
        )?;
        let _flock = acquire_storage_flock(profile_dir, STORAGE_LOCK_FILENAME)?;
        self.update_under_lock(f)
    }

    /// Apply one storage mutation while the caller already owns this profile's
    /// in-process save lock and cross-process storage flock.
    fn update_under_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        let (mut instances, mut groups) = self.load_with_groups()?;
        let groups_before = groups.clone();
        let result = f(&mut instances, &mut groups)?;

        // Pre-serialise both buffers so a serde failure on either side
        // aborts before any file is touched.
        let instances_buf = serde_json::to_vec_pretty(&instances)?;
        let groups_changed = groups != groups_before;
        let groups_buf = if groups_changed {
            Some(serde_json::to_vec_pretty(&groups)?)
        } else {
            None
        };

        // groups first, sessions last.
        if let Some(buf) = groups_buf {
            let groups_path = self.sessions_path.with_file_name("groups.json");
            atomic_write(&groups_path, &buf)?;
            self.file_watch.notify_local_change(&groups_path);
        }
        #[cfg(test)]
        if self.fail_writes_for_test {
            anyhow::bail!("injected sessions write failure");
        }

        atomic_write(&self.sessions_path, &instances_buf)?;
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(result)
    }

    /// Move one session, running `before_commit` only after the authoritative
    /// target validation succeeds while both profile locks are still held.
    pub(crate) fn move_instance_to_with_effect<F, B>(
        &self,
        target: &Storage,
        before: &Instance,
        after: &Instance,
        account_swap: bool,
        validate_target: F,
        before_commit: B,
    ) -> Result<Instance>
    where
        F: FnOnce(&[Instance], &Instance) -> Result<()>,
        B: FnOnce(&Instance) -> Result<()>,
    {
        let changes = [(before.clone(), after.clone())];
        let group_move = GroupMovePlan::single(&before.group_path, &after.group_path);
        let mut moved = self.move_instances_to_inner(
            target,
            &changes,
            MoveTransactionPlan {
                group_move: &group_move,
                merge_complete_post: true,
                account_swap,
            },
            |instances, candidates| validate_target(instances, &candidates[0]),
            |candidates| before_commit(&candidates[0]),
            sync_resolved_parent_directory,
        )?;
        Ok(moved.remove(0))
    }

    /// Move a batch between profiles as one dual-locked transaction.
    pub(crate) fn move_instances_to<F>(
        &self,
        target: &Storage,
        changes: &[(Instance, Instance)],
        group_move: &GroupMovePlan,
        validate_target: F,
    ) -> Result<Vec<Instance>>
    where
        F: FnOnce(&[Instance], &[Instance]) -> Result<()>,
    {
        self.move_instances_to_inner(
            target,
            changes,
            MoveTransactionPlan {
                group_move,
                merge_complete_post: false,
                account_swap: false,
            },
            validate_target,
            |_| Ok(()),
            sync_resolved_parent_directory,
        )
    }

    fn move_instances_to_inner<F, B, S>(
        &self,
        target: &Storage,
        changes: &[(Instance, Instance)],
        plan: MoveTransactionPlan<'_>,
        validate_target: F,
        before_commit: B,
        mut sync_target_parent: S,
    ) -> Result<Vec<Instance>>
    where
        F: FnOnce(&[Instance], &[Instance]) -> Result<()>,
        B: FnOnce(&[Instance]) -> Result<()>,
        S: FnMut(&Path) -> Result<()>,
    {
        if self.profile == target.profile {
            return Err(anyhow!("source and target profile are the same"));
        }
        let source_dir = self
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("source sessions path has no parent"))?
            .canonicalize()?;
        let target_dir = target
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("target sessions path has no parent"))?
            .canonicalize()?;
        if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical directory"
            ));
        }

        let (first, second) = if source_dir < target_dir {
            (self, target)
        } else {
            (target, self)
        };
        let _first_mu = first
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _second_mu = second
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_dir = first
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let second_dir = second
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let _transition_flocks =
            acquire_transition_flocks_for_profile_dirs(&[first_dir, second_dir])?;
        let (first_lock_file, first_lock_path) =
            open_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
        let (second_lock_file, second_lock_path) =
            open_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
        if same_filesystem_identity(&first_lock_file.metadata()?, &second_lock_file.metadata()?) {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical storage lock"
            ));
        }
        let _first_flock = acquire_open_storage_flock(first_lock_file, &first_lock_path)?;
        let _second_flock = acquire_open_storage_flock(second_lock_file, &second_lock_path)?;

        let source_groups_path = self.sessions_path.with_file_name("groups.json");
        let target_groups_path = target.sessions_path.with_file_name("groups.json");
        if existing_paths_share_filesystem_identity(&self.sessions_path, &target.sessions_path)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical sessions file"
            ));
        }
        if existing_paths_share_filesystem_identity(&source_groups_path, &target_groups_path)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical groups file"
            ));
        }

        let (mut source_instances, mut source_groups) = self.load_with_groups()?;
        let (mut target_instances, mut target_groups) = target.load_with_groups()?;
        let mut ids = std::collections::HashSet::with_capacity(changes.len());
        let mut moved = Vec::with_capacity(changes.len());
        for (before, after) in changes {
            if !ids.insert(before.id.as_str()) {
                return Err(anyhow!("duplicate session id in profile move batch"));
            }
            let source = source_instances
                .iter()
                .find(|instance| instance.id == before.id)
                .ok_or_else(|| anyhow!("Session not found in source profile"))?;
            if target_instances
                .iter()
                .any(|instance| instance.id == before.id)
            {
                return Err(anyhow!("Session already exists in target profile"));
            }
            let mut candidate = source.clone();
            if plan.merge_complete_post {
                candidate.merge_profile_move_diff(before, after, plan.account_swap);
            } else {
                candidate.merge_user_action_diff(before, after);
            }
            candidate.source_profile.clone_from(&target.profile);
            moved.push(candidate);
        }
        if plan.group_move.move_subtree {
            let source_prefix = format!("{}/", plan.group_move.source_path);
            let locked_members: std::collections::HashSet<&str> = source_instances
                .iter()
                .filter(|instance| {
                    instance.group_path == plan.group_move.source_path
                        || instance.group_path.starts_with(&source_prefix)
                })
                .map(|instance| instance.id.as_str())
                .collect();
            if locked_members != ids {
                return Err(anyhow!(
                    "group membership changed while the cross-profile move was pending"
                ));
            }
        }
        validate_target(&target_instances, &moved)?;

        let source_groups_before = serde_json::to_vec_pretty(&source_groups)?;
        let target_instances_before = serde_json::to_vec_pretty(&target_instances)?;
        let target_groups_before = serde_json::to_vec_pretty(&target_groups)?;

        let source_instances_before = serde_json::to_vec_pretty(&source_instances)?;
        source_instances.retain(|instance| !ids.contains(instance.id.as_str()));
        target_instances.extend(moved.iter().cloned());
        apply_group_move(
            plan.group_move,
            &source_instances,
            &mut source_groups,
            &target_instances,
            &mut target_groups,
        );
        let source_instances_after = serde_json::to_vec_pretty(&source_instances)?;
        let source_groups_after = serde_json::to_vec_pretty(&source_groups)?;
        let target_instances_after = serde_json::to_vec_pretty(&target_instances)?;
        let target_groups_after = serde_json::to_vec_pretty(&target_groups)?;
        let source_groups_changed = source_groups_after != source_groups_before;
        let target_groups_changed = target_groups_after != target_groups_before;
        let journal_entry = super::move_journal::MoveJournalEntry {
            version: super::move_journal::MOVE_JOURNAL_VERSION,
            ids: {
                let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
                ids.sort();
                ids
            },
            source_profile: self.profile.clone(),
            target_profile: target.profile.clone(),
            source_sessions_path: self.sessions_path.clone(),
            target_sessions_path: target.sessions_path.clone(),
            group_move_source_path: plan.group_move.source_path.clone(),
            group_move_target_path: plan.group_move.target_path.clone(),
            group_move_subtree: plan.group_move.move_subtree,
            created_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
        };
        let journal_path = super::move_journal::record(&journal_entry, &self.sessions_path)
            .context(
                "recording the durable move journal failed; no move effect or profile row changed",
            )?;
        #[cfg(test)]
        test_crash_point("profile-move-journal");
        // The durable journal precedes every mutation, including the external
        // worktree/container effect.
        before_commit(&moved)?;

        let resolved_target_groups_path = if target_groups_changed {
            Some(atomic_write_verified_resolved(
                &target_groups_path,
                &target_groups_after,
            )?)
        } else {
            None
        };
        let resolved_target_sessions_path = match atomic_write_verified_resolved(
            &target.sessions_path,
            &target_instances_after,
        ) {
            Ok(path) => path,
            Err(target_error) => {
                if target_groups_changed {
                    if let Err(rollback_error) = restore_file_durably(
                        &target_groups_path,
                        &target_groups_before,
                        || "target group rollback failed".to_string(),
                        || "target group rollback was not durable".to_string(),
                    ) {
                        return Err(anyhow!(
                            "target profile write failed ({target_error}); target group rollback also failed or was not durable ({rollback_error})"
                        ));
                    }
                }
                return Err(target_error);
            }
        };
        // `atomic_write` already syncs file content and attempts a directory sync.
        if let Some(path) = resolved_target_groups_path.as_deref() {
            sync_target_parent(path)?;
        }
        sync_target_parent(&resolved_target_sessions_path)?;
        #[cfg(test)]
        test_crash_point("profile-move-target");
        if target_groups_changed {
            target.file_watch.notify_local_change(&target_groups_path);
        }
        target.file_watch.notify_local_change(&target.sessions_path);

        if source_groups_changed {
            let source_group_result =
                atomic_write_verified(&source_groups_path, &source_groups_after)
                    .and_then(|()| sync_parent_directory(&source_groups_path));
            if let Err(source_group_error) = source_group_result {
                restore_file_durably(
                    &source_groups_path,
                    &source_groups_before,
                    || {
                        format!(
                            "source group write failed ({source_group_error}); source group rollback failed"
                        )
                    },
                    || {
                        format!(
                            "source group write failed ({source_group_error}); source group rollback was not durable"
                        )
                    },
                )?;
                restore_file_durably(
                    &target.sessions_path,
                    &target_instances_before,
                    || {
                        format!(
                            "source group write failed ({source_group_error}); target session rollback failed"
                        )
                    },
                    || {
                        format!(
                            "source group write failed ({source_group_error}); target session rollback was not durable"
                        )
                    },
                )?;
                if target_groups_changed {
                    restore_file_durably(
                        &target_groups_path,
                        &target_groups_before,
                        || {
                            format!(
                                "source group write failed ({source_group_error}); target group rollback failed"
                            )
                        },
                        || {
                            format!(
                                "source group write failed ({source_group_error}); target group rollback was not durable"
                            )
                        },
                    )?;
                }
                self.file_watch.notify_local_change(&source_groups_path);
                target.file_watch.notify_local_change(&target.sessions_path);
                if target_groups_changed {
                    target.file_watch.notify_local_change(&target_groups_path);
                }
                return Err(source_group_error);
            }
            #[cfg(test)]
            test_crash_point("profile-move-source-groups");
        }
        #[cfg(test)]
        test_crash_point("profile-move-source-sessions");
        if let Err(source_error) =
            atomic_write_verified(&self.sessions_path, &source_instances_after)
        {
            match self.load() {
                Ok(instances)
                    if moved
                        .iter()
                        .all(|candidate| instances.iter().all(|row| row.id != candidate.id)) =>
                {
                    tracing::warn!(target: "session.store", error = %source_error, "source profile write committed but could not be byte-verified");
                }
                Ok(_) => {
                    if source_groups_changed {
                        restore_file_durably(
                            &source_groups_path,
                            &source_groups_before,
                            || {
                                format!(
                                    "source session write failed ({source_error}); source group rollback failed"
                                )
                            },
                            || {
                                format!(
                                    "source session write failed ({source_error}); source group rollback was not durable"
                                )
                            },
                        )?;
                    }
                    restore_file_durably(
                        &target.sessions_path,
                        &target_instances_before,
                        || {
                            format!(
                                "source session write failed ({source_error}); target session rollback failed"
                            )
                        },
                        || {
                            format!(
                                "source session write failed ({source_error}); target session rollback was not durable"
                            )
                        },
                    )?;
                    if target_groups_changed {
                        restore_file_durably(
                            &target_groups_path,
                            &target_groups_before,
                            || {
                                format!(
                                    "source session write failed ({source_error}); target group rollback failed"
                                )
                            },
                            || {
                                format!(
                                    "source session write failed ({source_error}); target group rollback was not durable"
                                )
                            },
                        )?;
                    }
                    if source_groups_changed {
                        self.file_watch.notify_local_change(&source_groups_path);
                    }
                    target.file_watch.notify_local_change(&target.sessions_path);
                    if target_groups_changed {
                        target.file_watch.notify_local_change(&target_groups_path);
                    }
                    return Err(source_error);
                }
                Err(verify_error) => {
                    return Err(anyhow!(
                        "source profile write failed ({source_error}) and could not be verified ({verify_error}); target copies were retained"
                    ));
                }
            }
        }
        if let Err(sync_error) = sync_parent_directory(&self.sessions_path) {
            if source_groups_changed {
                restore_file_durably(
                    &source_groups_path,
                    &source_groups_before,
                    || {
                        format!(
                            "source session directory sync failed ({sync_error}); source group restore failed"
                        )
                    },
                    || {
                        format!(
                            "source session directory sync failed ({sync_error}); restored source groups were not durable"
                        )
                    },
                )?;
                self.file_watch.notify_local_change(&source_groups_path);
            }
            restore_file_durably(
                &self.sessions_path,
                &source_instances_before,
                || {
                    format!(
                        "source session directory sync failed ({sync_error}); source row restore failed"
                    )
                },
                || {
                    format!(
                        "source session directory sync failed ({sync_error}); restored source rows were not durable"
                    )
                },
            )?;
            self.file_watch.notify_local_change(&self.sessions_path);
            return Err(anyhow!(
                "source session removal was not durable ({sync_error}); source rows were restored and target copies retained"
            ));
        }
        // Every write and directory barrier above has passed.
        if let Err(error) = super::move_journal::consume(&journal_path) {
            tracing::warn!(
                target: "session.store",
                error = %error,
                "completed profile move could not consume its journal; recovery will discard it"
            );
        }
        if source_groups_changed {
            self.file_watch.notify_local_change(&source_groups_path);
        }
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(moved)
    }
}

// Workspace ordering is stored at the app-data root, not per-profile.
fn workspace_ordering_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("workspace-ordering.json"))
}

pub fn load_workspace_ordering() -> Result<WorkspaceOrdering> {
    let path = workspace_ordering_path()?;
    if !path.exists() {
        return Ok(WorkspaceOrdering::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(WorkspaceOrdering::default());
    }
    Ok(serde_json::from_str(&content)?)
}

/// Locked load -> mutate -> save for the global workspace ordering file.
pub fn update_workspace_ordering<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&mut WorkspaceOrdering) -> Result<R>,
{
    let _mu = workspace_ordering_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_dir = get_app_dir()?;
    let _flock = acquire_storage_flock(&app_dir, WORKSPACE_LOCK_FILENAME)?;
    let mut ordering = load_workspace_ordering()?;
    let result = f(&mut ordering)?;
    save_workspace_ordering(&ordering)?;
    Ok(result)
}

fn save_workspace_ordering(ordering: &WorkspaceOrdering) -> Result<()> {
    let path = workspace_ordering_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(ordering)?;
    atomic_write(&path, content.as_bytes())?;
    Ok(())
}

// Recent projects is a global most-recently-used store, written when a session is deleted so the
// project it lived in survives in the new-session wizard's Recent tab after its last session is
// gone.
const RECENT_PROJECTS_LOCK_FILENAME: &str = ".recent-projects.lock";
const RECENT_PROJECTS_CAP: usize = 20;

fn recent_projects_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn recent_projects_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("recent-projects.json"))
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct RecentProjectEntry {
    pub path: String,
    pub display_name: String,
    pub tool: String,
    /// RFC 3339, always UTC, so lexical order equals chronological order.
    pub last_used_at: String,
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct RecentProjects {
    projects: Vec<RecentProjectEntry>,
}

/// Build a recent-project entry from a session being deleted, or `None` for sessions that
/// must never appear in the wizard Recent list.
pub fn recent_project_entry_for(inst: &Instance) -> Option<RecentProjectEntry> {
    if inst.scratch || inst.workspace_info.is_some() {
        return None;
    }
    let raw = inst
        .worktree_info
        .as_ref()
        .map(|w| w.main_repo_path.as_str())
        .unwrap_or(inst.project_path.as_str());
    let trimmed = raw.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    // `file_name` resolves the basename with the host platform's separator rules, so a
    // Windows path like `C:\repo\proj` yields `proj` rather than the whole string.
    let display_name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string();
    let last_used_at = inst
        .last_accessed_at
        .unwrap_or(inst.created_at)
        .to_rfc3339();
    Some(RecentProjectEntry {
        path: path.to_string(),
        display_name,
        tool: inst.tool.clone(),
        last_used_at,
    })
}

/// Upsert a recently used project, keyed by normalized path (newest `last_used_at` wins),
/// capped to the most recent `RECENT_PROJECTS_CAP`.
pub fn record_recent_project(entry: RecentProjectEntry) -> Result<()> {
    let _mu = recent_projects_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_dir = get_app_dir()?;
    let _flock = acquire_storage_flock(&app_dir, RECENT_PROJECTS_LOCK_FILENAME)?;
    let mut store = load_recent_projects_inner()?;
    store.projects.retain(|p| p.path != entry.path);
    store.projects.push(entry);
    store
        .projects
        .sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
    store.projects.truncate(RECENT_PROJECTS_CAP);
    save_recent_projects(&store)?;
    Ok(())
}

/// Persisted recent projects, newest first.
pub fn load_recent_projects() -> Result<Vec<RecentProjectEntry>> {
    Ok(load_recent_projects_inner()?.projects)
}

fn load_recent_projects_inner() -> Result<RecentProjects> {
    let path = recent_projects_path()?;
    if !path.exists() {
        return Ok(RecentProjects::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(RecentProjects::default());
    }
    Ok(serde_json::from_str(&content)?)
}

fn save_recent_projects(store: &RecentProjects) -> Result<()> {
    let path = recent_projects_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store)?;
    atomic_write(&path, content.as_bytes())?;
    Ok(())
}

/// Outcome of one reconciliation pass over the loaded profiles.
#[derive(Debug, Default)]
pub(crate) struct ReconciliationOutcome {
    /// True when at least one journal-guided repair changed durable state, so
    /// the caller must reload from disk before publishing anything.
    pub(crate) repaired: bool,
    /// Duplicates that lack arbitration evidence and remain excluded.
    pub(crate) reports: Vec<DuplicateIdReport>,
}

/// One ambiguous copy of a duplicated session id.
#[derive(Debug, Clone)]
pub(crate) struct DuplicateCopy {
    pub(crate) profile: String,
    pub(crate) sessions_path: PathBuf,
    pub(crate) modified_at_epoch_ms: Option<u64>,
}

/// A duplicate id that could not be repaired automatically.
#[derive(Debug, Clone)]
pub(crate) struct DuplicateIdReport {
    pub(crate) id: String,
    pub(crate) copies: Vec<DuplicateCopy>,
}

impl DuplicateIdReport {
    /// Single-line, user-actionable summary naming every copy's profile, store file, and
    /// mtime.
    pub(crate) fn actionable_message(&self) -> String {
        let copies = self
            .copies
            .iter()
            .map(|copy| {
                let modified = copy
                    .modified_at_epoch_ms
                    .map(|ms| format!("mtime {ms}ms"))
                    .unwrap_or_else(|| "unknown mtime".to_string());
                format!(
                    "profile `{}` at {} ({modified})",
                    copy.profile,
                    copy.sessions_path.display()
                )
            })
            .collect::<Vec<String>>()
            .join(" and ");
        format!(
            "session id `{}` exists in multiple profiles without a usable move journal; \
             nothing was changed automatically. Resolve it manually by keeping one copy \
             and deleting the other from its sessions.json (and groups.json sidecar): {copies}",
            self.id
        )
    }
}
fn file_mtime_epoch_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

/// Ids appearing more than once across `loaded` (within one profile or
/// across profiles), in deterministic first-seen order.
pub(crate) fn detect_duplicate_ids<'a>(
    loaded: impl IntoIterator<Item = (&'a str, &'a [Instance])>,
) -> Vec<String> {
    // Counts occurrences across every profile; an id repeated even within one profile is ambiguous
    // the same way (corrupt file or writer bug) and must surface, not silently fail closed.
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (_, instances) in loaded {
        for instance in instances {
            let count = counts.entry(instance.id.as_str()).or_insert_with(|| {
                order.push(instance.id.clone());
                0
            });
            *count += 1;
        }
    }
    order
        .into_iter()
        .filter(|id| counts[id.as_str()] > 1)
        .collect()
}

/// Journal evidence older than this is insufficient for arbitration.
const MOVE_JOURNAL_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// How long after journal creation a store mtime still counts as part of the crashed
/// transaction itself rather than a later user edit.
const MOVE_JOURNAL_MTIME_SLACK_MS: u64 = 5 * 60 * 1000;

/// Paths already reported as unusable this process lifetime, so a permanently
/// broken entry cannot produce ERROR spam and lock churn on every reload tick.
static UNUSABLE_JOURNAL_ENTRIES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn unusable_journal_entries_contains(path: &Path) -> bool {
    UNUSABLE_JOURNAL_ENTRIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|seen| seen == path)
}

/// Paths whose repair failure has already been reported at ERROR level, so a
/// persistently failing (retrying) repair logs once per process.
static REPAIR_FAILURES_REPORTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn mark_repair_failure_logged(path: &Path) -> bool {
    let mut seen = REPAIR_FAILURES_REPORTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.iter().any(|seen| seen == path) {
        false
    } else {
        seen.push(path.to_path_buf());
        true
    }
}

fn mark_unusable_journal_entry(path: &Path) {
    let mut seen = UNUSABLE_JOURNAL_ENTRIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !seen.iter().any(|seen| seen == path) {
        seen.push(path.to_path_buf());
    }
}

fn entry_age(entry: &super::move_journal::MoveJournalEntry) -> std::time::Duration {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    std::time::Duration::from_millis(now_ms.saturating_sub(entry.created_at_epoch_ms))
}

/// Detect duplicates across the loaded profiles, run journal-guided repair for the cases
/// with durable evidence, and return reports for whatever remains ambiguous.
pub(crate) fn reconcile_profile_duplicates(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> ReconciliationOutcome {
    let mut outcome = ReconciliationOutcome::default();
    // Normalize to a name-sorted view so report and copy ordering are
    // deterministic regardless of how the caller iterates its storages.
    let mut normalized: Vec<(&str, &[Instance])> = loaded.to_vec();
    normalized.sort_by(|left, right| left.0.cmp(right.0));
    let duplicated = !detect_duplicate_ids(normalized.iter().copied()).is_empty();
    let scan = super::move_journal::scan(
        storages
            .iter()
            .map(|(_, storage)| storage.sessions_path().to_path_buf()),
    );
    let mut opaque_failure = !scan.unreadable_dirs.is_empty();
    for (dir, error) in scan.unreadable_dirs {
        tracing::warn!(
            target: "session.store",
            path = %dir.display(),
            error = %error,
            "move journal directory could not be listed; all arbitration is deferred"
        );
    }
    let mut valid_entries = Vec::new();
    for (path, parsed) in scan.entries {
        match parsed {
            Ok(entry) => valid_entries.push((path, entry)),
            Err(reason) => {
                opaque_failure = true;
                let already_reported = UNUSABLE_JOURNAL_ENTRIES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .any(|unusable| unusable == &path);
                if !already_reported {
                    mark_unusable_journal_entry(&path);
                    tracing::error!(
                        target: "session.store",
                        path = %path.display(),
                        reason = %reason,
                        "opaque move journal evidence blocks arbitration for this reload"
                    );
                }
            }
        }
    }
    valid_entries.sort_by_key(|(path, entry)| {
        std::cmp::Reverse((
            entry.created_at_epoch_ms,
            super::move_journal::file_created_at_nanos(path).unwrap_or_default(),
        ))
    });
    if valid_entries.is_empty() && !duplicated {
        return outcome;
    }
    if !opaque_failure {
        let mut blocked_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (path, entry) in valid_entries {
            if entry.ids.iter().any(|id| blocked_ids.contains(id)) {
                // Shadowing is transitive across a multi-id batch.
                blocked_ids.extend(entry.ids.iter().cloned());
                tracing::debug!(
                    target: "session.store",
                    path = %path.display(),
                    "older move journal is shadowed by unresolved newer intent"
                );
                continue;
            }
            let already_unusable = UNUSABLE_JOURNAL_ENTRIES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|unusable| unusable == &path);
            if already_unusable {
                blocked_ids.extend(entry.ids.iter().cloned());
                continue;
            }
            if entry_age(&entry) > MOVE_JOURNAL_MAX_AGE {
                mark_unusable_journal_entry(&path);
                blocked_ids.extend(entry.ids.iter().cloned());
                tracing::error!(
                    target: "session.store",
                    path = %path.display(),
                    ids = %entry.ids.join(","),
                    "expired move journal entry is insufficient evidence for arbitration; duplicates stay surfaced"
                );
                continue;
            }
            match repair_journal_entry(&entry, storages, &path) {
                Ok(true) => {
                    outcome.repaired = true;
                    tracing::info!(
                        target: "session.store",
                        ids = %entry.ids.join(","),
                        "reconciled interrupted profile move from its journal"
                    );
                }
                Ok(false) => {
                    blocked_ids.extend(entry.ids.iter().cloned());
                    tracing::debug!(
                        target: "session.store",
                        path = %path.display(),
                        "newer unresolved move intent blocks older overlapping journals"
                    );
                }
                Err(error) => {
                    blocked_ids.extend(entry.ids.iter().cloned());
                    if mark_repair_failure_logged(&path) {
                        tracing::error!(
                            target: "session.store",
                            path = %path.display(),
                            error = %error,
                            "journal-guided repair failed; older overlapping intent is blocked"
                        );
                    } else {
                        tracing::debug!(
                            target: "session.store",
                            path = %path.display(),
                            error = %error,
                            "journal-guided repair failed again"
                        );
                    }
                }
            }
        }
    }
    if !outcome.repaired {
        // Nothing changed on disk.
        outcome.reports = duplicate_reports(&normalized, storages);
        return outcome;
    }
    let (reports, reload_succeeded) = reports_after_repair(&normalized, storages);
    outcome.reports = reports;
    if !reload_succeeded {
        // Keep the caller on its pre-repair loads so fail-closed reports can be
        // published instead of immediately repeating the same failed reload.
        outcome.repaired = false;
    }
    outcome
}

fn reports_after_repair(
    fallback: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> (Vec<DuplicateIdReport>, bool) {
    let mut reloaded: Vec<(String, Vec<Instance>)> = Vec::with_capacity(storages.len());
    for (_, storage) in storages {
        match storage.load() {
            Ok(instances) => reloaded.push((storage.profile.clone(), instances)),
            Err(error) => {
                tracing::error!(
                    target: "session.store",
                    profile = %storage.profile,
                    error = %error,
                    "post-repair reload failed; preserving the pre-repair duplicate report"
                );
                return (duplicate_reports(fallback, storages), false);
            }
        }
    }
    reloaded.sort_by(|left, right| left.0.cmp(&right.0));
    let reloaded_refs: Vec<(&str, &[Instance])> = reloaded
        .iter()
        .map(|(profile, instances)| (profile.as_str(), instances.as_slice()))
        .collect();
    (duplicate_reports(&reloaded_refs, storages), true)
}

/// Build one report per duplicated id with per-copy profile, store path, and mtime.
fn duplicate_reports(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> Vec<DuplicateIdReport> {
    let mut reports: Vec<DuplicateIdReport> = Vec::new();
    for id in detect_duplicate_ids(loaded.iter().copied()) {
        let copies = loaded
            .iter()
            .filter(|(_, instances)| instances.iter().any(|instance| instance.id == id))
            .filter_map(|(profile, _)| {
                let profile = *profile;
                storages
                    .iter()
                    .find(|(name, _)| *name == profile)
                    .map(|(_, storage)| storage)
            })
            .map(|storage| DuplicateCopy {
                profile: storage.profile.clone(),
                sessions_path: storage.sessions_path().to_path_buf(),
                modified_at_epoch_ms: file_mtime_epoch_ms(storage.sessions_path()),
            })
            .collect();
        reports.push(DuplicateIdReport { id, copies });
    }
    reports
}

/// True when `candidate` equals or contains `path` as an ancestor segment.
fn group_path_covers(candidate: &str, path: &str) -> bool {
    candidate == path || path.starts_with(&format!("{candidate}/"))
}

fn validate_recovery_journal(
    entry: &super::move_journal::MoveJournalEntry,
    source: &Storage,
    target: &Storage,
) -> Result<Option<String>> {
    let mut ids = std::collections::HashSet::with_capacity(entry.ids.len());
    for id in &entry.ids {
        if let Err(error) = super::validate_instance_id(id) {
            return Ok(Some(format!("invalid session id {id:?}: {error}")));
        }
        if !ids.insert(id.as_str()) {
            return Ok(Some(format!(
                "duplicate session id {id:?} in journal batch"
            )));
        }
    }
    if std::ptr::eq(source, target) || entry.source_profile == entry.target_profile {
        return Ok(Some(
            "source and target resolve to the same loaded store".to_string(),
        ));
    }
    let source_dir = source
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("source sessions path has no parent"))?
        .canonicalize()?;
    let target_dir = target
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("target sessions path has no parent"))?
        .canonicalize()?;
    if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
        return Ok(Some(
            "source and target resolve to the same physical profile directory".to_string(),
        ));
    }
    if existing_paths_share_filesystem_identity(source.sessions_path(), target.sessions_path())? {
        return Ok(Some(
            "source and target resolve to the same physical sessions file".to_string(),
        ));
    }
    Ok(None)
}

fn with_two_storage_locks<F, R>(source: &Storage, target: &Storage, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    let source_dir = source
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("source sessions path has no parent"))?
        .canonicalize()?;
    let target_dir = target
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("target sessions path has no parent"))?
        .canonicalize()?;
    if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
        anyhow::bail!("source and target resolve to the same physical profile directory");
    }
    let (first, second) = if source_dir < target_dir {
        (source, target)
    } else {
        (target, source)
    };
    let _first_mu = first
        .save_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _second_mu = second
        .save_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first_dir = first.sessions_path.parent().unwrap();
    let second_dir = second.sessions_path.parent().unwrap();
    let _transition_flocks = acquire_transition_flocks_for_profile_dirs(&[first_dir, second_dir])?;
    let (first_file, first_path) = open_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
    let (second_file, second_path) = open_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
    if same_filesystem_identity(&first_file.metadata()?, &second_file.metadata()?) {
        anyhow::bail!("source and target resolve to the same physical storage lock");
    }
    let _first_flock = acquire_open_storage_flock(first_file, &first_path)?;
    let _second_flock = acquire_open_storage_flock(second_file, &second_path)?;
    f()
}

/// Apply the winner policy to one journal entry.
fn repair_journal_entry(
    entry: &super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
) -> Result<bool> {
    repair_journal_entry_with_sync(entry, storages, journal_path, sync_parent_directory)
}

fn repair_journal_entry_with_sync<S>(
    entry: &super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
    mut sync: S,
) -> Result<bool>
where
    S: FnMut(&Path) -> Result<()>,
{
    let source_storage =
        match resolve_journal_store(&entry.source_profile, &entry.source_sessions_path, storages) {
            Some(storage) => storage,
            None => return Ok(false),
        };
    let target_storage =
        match resolve_journal_store(&entry.target_profile, &entry.target_sessions_path, storages) {
            Some(storage) => storage,
            None => return Ok(false),
        };

    if let Some(reason) = validate_recovery_journal(entry, source_storage, target_storage)? {
        mark_unusable_journal_entry(journal_path);
        tracing::error!(
            target: "session.store",
            path = %journal_path.display(),
            reason = %reason,
            "move journal entry is semantically invalid; duplicates stay surfaced"
        );
        return Ok(false);
    }

    for storage in [source_storage, target_storage] {
        let mtime = file_mtime_epoch_ms(storage.sessions_path()).unwrap_or_default();
        if mtime.saturating_sub(entry.created_at_epoch_ms) > MOVE_JOURNAL_MTIME_SLACK_MS {
            mark_unusable_journal_entry(journal_path);
            tracing::warn!(
                target: "session.store",
                path = %journal_path.display(),
                ids = %entry.ids.join(","),
                "store was modified after the move journal was written; entry is permanently insufficient evidence and stays surfaced"
            );
            return Ok(false);
        }
    }

    // App-global identity lock first, then sorted title/lifecycle locks, then the
    // per-profile storage flocks taken inside `Storage::update`.
    let _identity_lock = acquire_session_identity_lock()?;
    let mut ids_sorted = entry.ids.clone();
    ids_sorted.sort();
    ids_sorted.dedup();
    let mut guards = Vec::with_capacity(ids_sorted.len() * 2);
    for id in &ids_sorted {
        guards.push(acquire_session_title_lock(id)?);
    }
    let source_scope = source_storage
        .sessions_path()
        .parent()
        .unwrap()
        .canonicalize()?;
    let target_scope = target_storage
        .sessions_path()
        .parent()
        .unwrap()
        .canonicalize()?;
    for id in &ids_sorted {
        guards.push(source_storage.acquire_instance_lifecycle_lock(id)?);
        if source_scope != target_scope {
            guards.push(target_storage.acquire_instance_lifecycle_lock(id)?);
        }
    }

    with_two_storage_locks(source_storage, target_storage, || {
        let (source_instances, _source_groups) = source_storage.load_with_groups()?;
        let (target_instances, _) = target_storage.load_with_groups()?;
        let plan = crate::session::GroupMovePlan {
            source_path: entry.group_move_source_path.clone(),
            target_path: entry.group_move_target_path.clone(),
            move_subtree: entry.group_move_subtree,
        };
        // Automatic arbitration requires one valid row on each side.
        if entry.ids.iter().any(|id| {
            source_instances.iter().filter(|row| &row.id == id).count() > 1
                || target_instances.iter().filter(|row| &row.id == id).count() > 1
        }) {
            return Ok(false);
        }
        let source_losers: Vec<String> = entry
            .ids
            .iter()
            .filter(|id| {
                target_instances.iter().any(|row| &row.id == *id)
                    && source_instances.iter().any(|row| &row.id == *id)
            })
            .cloned()
            .collect();
        if source_losers.is_empty() {
            sync_repaired_profile_durably(source_storage, &mut sync)?;
            super::move_journal::consume(journal_path)?;
            return Ok(true);
        }

        source_storage.update_under_lock(|instances, groups| {
            if !target_still_holds(target_storage.sessions_path(), &source_losers)? {
                anyhow::bail!(
                    "target copies vanished while the repair was starting; leaving the journal for a retry"
                );
            }
            backup_before_repair(source_storage.sessions_path())?;
            backup_before_repair(&source_storage.sessions_path().with_file_name("groups.json"))?;
            instances.retain(|row| !source_losers.contains(&row.id));
            let winners: Vec<crate::session::Instance> = instances
                .iter()
                .filter(|row| entry.ids.contains(&row.id))
                .cloned()
                .collect();
            reconcile_groups_after_repair(instances, groups, &winners, &plan);
            Ok(())
        })?;
        sync_repaired_profile_durably(source_storage, &mut sync)?;
        #[cfg(test)]
        test_crash_point("profile-repair-source-written");
        super::move_journal::consume(journal_path)?;
        Ok(true)
    })
}

fn sync_repaired_profile_durably<S>(storage: &Storage, mut sync: S) -> Result<()>
where
    S: FnMut(&Path) -> Result<()>,
{
    // The two files normally share a profile directory, but supported symlinks may resolve
    // them into different directories.
    sync(storage.sessions_path()).context("repaired sessions directory was not made durable")?;
    sync(&storage.sessions_path().with_file_name("groups.json"))
        .context("repaired groups directory was not made durable")
}

/// True when the target sessions file currently holds every loser id.
fn target_still_holds(target_sessions_path: &Path, losers: &[String]) -> Result<bool> {
    // Two-phase parse mirroring `Storage::load`.
    let content = match fs::read_to_string(target_sessions_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("failed re-reading target sessions during repair"),
    };
    let rows: Vec<serde_json::Value> = serde_json::from_str(&content)
        .context("failed parsing target sessions during repair re-check")?;
    let held: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            <Instance as serde::Deserialize>::deserialize(row)
                .ok()
                .map(|instance| instance.id)
        })
        .collect();
    Ok(losers
        .iter()
        .all(|loser| held.iter().any(|row_id| row_id == loser)))
}

/// Resolve one journal-recorded profile only when the loaded store still
/// names the same sessions file (including symlink aliases).
fn resolve_journal_store<'a>(
    profile: &str,
    recorded_path: &Path,
    storages: &'a [(&str, &'a Storage)],
) -> Option<&'a Storage> {
    let storage = storages
        .iter()
        .find(|(name, _)| *name == profile)
        .map(|(_, storage)| *storage)?;
    if storage.sessions_path() == recorded_path {
        return Some(storage);
    }
    // Tolerate symlinked or differently-spelled paths when they still resolve
    // to the same physical file.
    match (
        recorded_path.canonicalize(),
        storage.sessions_path().canonicalize(),
    ) {
        (Ok(recorded), Ok(live)) if recorded == live => Some(storage),
        _ => None,
    }
}

const RECOVERY_BACKUPS_TO_KEEP: usize = 3;

/// Back up one repaired file and keep only the newest bounded set for that filename.
fn backup_before_repair(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).context(format!("failed reading {} for backup", path.display()))
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut name = path
        .file_name()
        .expect("sessions path has a file name")
        .to_os_string();
    name.push(format!(".pre-recovery-{stamp}"));
    let backup = path.with_file_name(name);
    atomic_write_verified(&backup, &bytes)?;
    sync_parent_directory(&backup)
        .context(format!("backup {} was not made durable", backup.display()))?;
    prune_old_recovery_backups(path, RECOVERY_BACKUPS_TO_KEEP)
}

fn prune_old_recovery_backups(path: &Path, keep: usize) -> Result<()> {
    prune_old_recovery_backups_with_sync(path, keep, sync_resolved_parent_directory)
}

fn prune_old_recovery_backups_with_sync<S>(path: &Path, keep: usize, mut sync: S) -> Result<()>
where
    S: FnMut(&Path) -> Result<()>,
{
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}.pre-recovery-");
    let mut backups: Vec<(u128, PathBuf)> = fs::read_dir(parent)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|candidate| {
            let timestamp = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|stamp| stamp.parse::<u128>().ok())?;
            Some((timestamp, candidate))
        })
        .collect();
    backups.sort_by_key(|(timestamp, _)| *timestamp);
    let remove_count = backups.len().saturating_sub(keep);
    if remove_count == 0 {
        return Ok(());
    }
    for (_, old) in backups.into_iter().take(remove_count) {
        fs::remove_file(&old)
            .with_context(|| format!("failed pruning old recovery backup {}", old.display()))?;
    }
    // Backup files are lexical siblings of path even when path itself is a symlink.
    sync(path).context("recovery backup pruning was not made durable")
}

/// Keep the repaired profile's groups sidecar consistent with what an uninterrupted
/// `apply_group_move` would have left on disk.
fn reconcile_groups_after_repair(
    instances: &[Instance],
    groups: &mut Vec<Group>,
    winners: &[Instance],
    plan: &crate::session::GroupMovePlan,
) {
    let source_prefix = format!("{}/", plan.source_path);
    let attributable = |path: &str| {
        !plan.source_path.is_empty()
            && (path == plan.source_path || (plan.move_subtree && path.starts_with(&source_prefix)))
    };
    let has_member = |path: &str| {
        instances
            .iter()
            .any(|instance| group_path_covers(path, &instance.group_path))
    };
    // Mirror apply_group_move's non-subtree branch.
    let existing_paths: Vec<String> = groups.iter().map(|group| group.path.clone()).collect();
    let has_explicit_descendant = |path: &str| {
        let prefix = format!("{path}/");
        existing_paths
            .iter()
            .any(|candidate| candidate.starts_with(&prefix))
    };
    groups.retain(|group| {
        !attributable(&group.path)
            || has_member(&group.path)
            || (!plan.move_subtree && has_explicit_descendant(&group.path))
    });
    for winner in winners {
        if winner.group_path.is_empty() {
            continue;
        }
        let leaf = winner
            .group_path
            .rsplit('/')
            .next()
            .unwrap_or(&winner.group_path);
        if !groups.iter().any(|group| group.path == winner.group_path) {
            groups.push(Group::new(leaf, &winner.group_path));
        }
    }
    // Same final re-tree `apply_group_move` performs, so the durable sidecar
    // carries the full explicit ancestor chain with preserved metadata.
    *groups = super::GroupTree::new_with_groups(instances, groups).get_all_groups();
}

#[cfg(test)]
mod tests {
    use super::super::move_journal;
    use super::*;
    use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::GroupTree;
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup_test_home(temp: &std::path::Path) -> AppDirGuard {
        isolate_app_dir_at(temp)
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn is_symlink(path: &Path) -> bool {
        fs::symlink_metadata(path).unwrap().file_type().is_symlink()
    }

    fn increment(path: &Path, fail: bool) -> Result<u64> {
        locked_update(
            path,
            |s| Ok(s.trim().parse::<u64>()?),
            |v| Ok(v.to_string()),
            |v| {
                let seen = *v;
                *v += 1;
                if fail {
                    return Err(anyhow!("validation failed after mutating"));
                }
                Ok(seen)
            },
        )?
    }

    fn seed(storage: &Storage, instances: &[Instance]) -> Result<()> {
        storage.update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(instances, &[]).get_all_groups();
            Ok(())
        })
    }

    #[test]
    fn locked_update_contract() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        assert_eq!(
            increment(&path, false).unwrap(),
            0,
            "missing file parses as default"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "1");

        fs::write(&path, "41").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(increment(&path, false).unwrap(), 41);
        assert_eq!(fs::read_to_string(&path).unwrap(), "42");
        #[cfg(unix)]
        assert_eq!(
            mode(&path),
            0o600,
            "data file must be re-tightened owner-only"
        );

        assert!(increment(&path, true).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "42",
            "a failed mutation must not persist half-applied state"
        );

        #[cfg(unix)]
        {
            let link = tmp.path().join("link.txt");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            increment(&link, false).unwrap();
            assert!(is_symlink(&link));
            assert_eq!(fs::read_to_string(&path).unwrap(), "43");
        }
    }

    #[test]
    fn locked_update_concurrent_writers_lose_no_updates() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        increment(&path, false).unwrap();
                    }
                });
            }
        });
        assert_eq!(fs::read_to_string(&path).unwrap(), "100");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_symlinks_and_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("real-config.toml");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        atomic_write(&link, b"new").unwrap();
        assert!(
            is_symlink(&link),
            "a rename over the link would desync dotfiles"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(mode(&target), 0o644);

        let missing = tmp.path().join("not-yet.toml");
        let dangling = tmp.path().join("dangling.toml");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        atomic_write(&dangling, b"seeded").unwrap();
        assert_eq!(fs::read_to_string(&missing).unwrap(), "seeded");
        assert!(is_symlink(&dangling));
    }

    #[cfg(unix)]
    #[test]
    fn replace_file_no_follow_refuses_planted_links() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("bind");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(root.join("agent/extensions")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("host-secret");
        fs::write(&secret, "untouched").unwrap();
        let rel = Path::new("agent/extensions/extension.js");

        std::os::unix::fs::symlink(&secret, root.join(rel)).unwrap();
        replace_file_no_follow(&root, rel, b"payload").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(root.join(rel)).unwrap(), "payload");
        assert!(!fs::symlink_metadata(root.join(rel))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o777,
            0o644,
            "the container reader may not be the uid that owns the bind"
        );

        fs::remove_dir_all(root.join("agent")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("agent")).unwrap();
        let err = replace_file_no_follow(&root, rel, b"payload").unwrap_err();
        assert!(
            !outside.join("extensions").exists(),
            "a swapped ancestor must not be traversed: {err:#}"
        );

        fs::remove_file(root.join("agent")).unwrap();
        let leftovers: Vec<_> = fs::read_dir(root.join("agent/extensions"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|name| name != "extension.js")
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn read_file_no_follow_reads_only_regular_files_below_root() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("bind");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("host-secret");
        fs::write(&secret, "host-only").unwrap();

        let rel = Path::new("agent/config.json");
        assert_eq!(read_file_no_follow(&root, rel).unwrap(), None, "missing");
        assert_eq!(
            read_file_no_follow(&tmp.path().join("no-such-bind"), rel).unwrap(),
            None,
            "missing root"
        );

        fs::write(root.join(rel), "inside").unwrap();
        assert_eq!(
            read_file_no_follow(&root, rel).unwrap().as_deref(),
            Some("inside")
        );

        fs::remove_file(root.join(rel)).unwrap();
        std::os::unix::fs::symlink(&secret, root.join(rel)).unwrap();
        assert_eq!(
            read_file_no_follow(&root, rel).unwrap(),
            None,
            "planted link"
        );

        fs::remove_file(root.join(rel)).unwrap();
        nix::unistd::mkfifo(
            &root.join(rel),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        assert_eq!(read_file_no_follow(&root, rel).unwrap(), None, "fifo");
        fs::remove_file(root.join(rel)).unwrap();
        fs::create_dir(root.join(rel)).unwrap();
        assert_eq!(read_file_no_follow(&root, rel).unwrap(), None, "directory");

        fs::write(outside.join("config.json"), "host-only").unwrap();
        fs::remove_dir_all(root.join("agent")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("agent")).unwrap();
        assert_eq!(
            read_file_no_follow(&root, rel).unwrap(),
            None,
            "linked parent"
        );

        assert!(
            read_file_no_follow(&root, Path::new("../outside/host-secret")).is_err(),
            "a path that leaves the anchor is rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_symlink_chain_cases() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let p = |name: &str| tmp.path().join(name);
        let chain = |len: usize| {
            let target = p(&format!("target-{len}"));
            fs::write(&target, b"x").unwrap();
            let mut prev = target.clone();
            for i in 0..len {
                let link = p(&format!("chain-{len}-{i}"));
                symlink(&prev, &link).unwrap();
                prev = link;
            }
            (target, prev)
        };

        assert_eq!(resolve_symlink_chain(&p("missing")).unwrap(), p("missing"));
        fs::write(p("regular"), b"x").unwrap();
        assert_eq!(resolve_symlink_chain(&p("regular")).unwrap(), p("regular"));
        symlink("regular", p("relative")).unwrap();
        symlink(p("relative"), p("top")).unwrap();
        assert_eq!(resolve_symlink_chain(&p("top")).unwrap(), p("regular"));
        symlink(p("nowhere"), p("dangling")).unwrap();
        assert_eq!(resolve_symlink_chain(&p("dangling")).unwrap(), p("nowhere"));
        let (target, head) = chain(32);
        assert_eq!(resolve_symlink_chain(&head).unwrap(), target);

        symlink(p("b"), p("a")).unwrap();
        symlink(p("a"), p("b")).unwrap();
        for head in [p("a"), chain(33).1] {
            let err = resolve_symlink_chain(&head).unwrap_err().to_string();
            assert!(err.contains("too deep"), "got: {err}");
        }
    }

    #[test]
    #[serial]
    fn storage_round_trips_sessions_and_groups() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;
        let groups_path = storage.sessions_path.with_file_name("groups.json");

        assert!(storage.load()?.is_empty(), "missing file loads empty");
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        for blank in ["", "   \n  \t  "] {
            fs::write(&storage.sessions_path, blank)?;
            assert!(storage.load()?.is_empty());
        }
        fs::write(&storage.sessions_path, "{ invalid json }")?;
        assert!(storage.load().is_err());

        // `update` reads before it writes, so clear the corrupt file first.
        fs::write(&storage.sessions_path, "[]")?;
        seed(&storage, &[])?;
        assert_eq!(fs::read_to_string(&storage.sessions_path)?.trim(), "[]");

        let mut instance = Instance::new("Test Project", "/home/user/project");
        instance.tool = "opencode".to_string();
        instance.command = "opencode --config test".to_string();
        instance.group_path = "work/clients".to_string();
        for i in 0..3 {
            let second = Instance::new(&format!("iter{i}"), "/tmp/test");
            seed(&storage, &[instance.clone(), second])?;
        }
        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 2);
        let first = &loaded[0];
        assert_eq!(
            (
                &*first.title,
                &*first.project_path,
                &*first.tool,
                &*first.command
            ),
            (
                "Test Project",
                "/home/user/project",
                "opencode",
                "opencode --config test"
            )
        );
        assert_eq!(first.group_path, "work/clients");
        assert_eq!(loaded[1].title, "iter2");
        let entries: Vec<_> = fs::read_dir(storage.sessions_path.parent().unwrap())?
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().all(|e| !e.contains(".tmp")),
            "atomic_write leaked temp files: {entries:?}"
        );

        fs::write(&groups_path, "   ")?;
        let (instances, groups) = storage.load_with_groups()?;
        assert_eq!(instances.len(), 2);
        assert!(groups.is_empty());

        storage.update(|_, groups| {
            groups.push(Group::new("projects", "work/projects"));
            Ok(())
        })?;
        let (_, groups) = storage.load_with_groups()?;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].path, "work/projects");
        Ok(())
    }

    #[test]
    #[serial]
    fn open_unwatched_requires_existing_profile() {
        let temp = tempdir().unwrap();
        let guard = setup_test_home(temp.path());
        let profile_dir = guard.path().join("profiles").join("ghost");

        let err = Storage::open_unwatched("ghost")
            .err()
            .expect("unknown profile");
        assert!(err.to_string().contains("does not exist"), "got: {err}");
        assert!(!profile_dir.exists(), "must not create the profile dir");

        crate::session::create_profile("known").unwrap();
        assert_eq!(Storage::open_unwatched("known").unwrap().profile(), "known");
    }

    #[test]
    #[serial]
    fn corrupt_rows_are_quarantined_and_top_level_corruption_errors() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        let sessions_q = storage
            .sessions_path
            .with_file_name("sessions.corrupt.jsonl");
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let groups_q = storage.sessions_path.with_file_name("groups.corrupt.jsonl");

        for bad in [&b"{}"[..], &b"{ this is not valid json ]"[..]] {
            fs::write(&storage.sessions_path, bad)?;
            assert!(storage.load().is_err());
            fs::write(&storage.sessions_path, "[]")?;
            fs::write(&groups_path, bad)?;
            assert!(storage.load_with_groups().is_err());
        }
        assert!(!sessions_q.exists() && !groups_q.exists());

        let sessions = serde_json::json!([
            Instance::new("alpha", "/tmp/alpha"),
            { "title": "corrupt-no-id" },
            Instance::new("beta", "/tmp/beta"),
        ]);
        let groups = serde_json::json!([
            Group::new("alpha", "work/alpha"),
            { "name": "corrupt-no-path" },
            Group::new("beta", "work/beta"),
        ]);
        fs::write(&storage.sessions_path, serde_json::to_vec(&sessions)?)?;
        fs::write(&groups_path, serde_json::to_vec(&groups)?)?;

        for _ in 0..2 {
            let (instances, groups) = storage.load_with_groups()?;
            let titles: Vec<_> = instances.iter().map(|i| i.title.as_str()).collect();
            let paths: Vec<_> = groups.iter().map(|g| g.path.as_str()).collect();
            assert_eq!(titles, ["alpha", "beta"]);
            assert_eq!(paths, ["work/alpha", "work/beta"]);
            assert_eq!(storage.load()?.len(), 2);
            for (quarantine, needle) in [
                (&sessions_q, "corrupt-no-id"),
                (&groups_q, "corrupt-no-path"),
            ] {
                let q = fs::read_to_string(quarantine)?;
                assert_eq!(q.lines().count(), 1, "repeated load must not duplicate");
                assert!(q.contains(needle));
                #[cfg(unix)]
                assert_eq!(mode(quarantine), 0o600);
            }
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn empty_profile_argument_resolves_default_profile() -> Result<()> {
        for (existing, configured, expected) in [
            (&[][..], None, "main"),
            (&["work", "personal"][..], None, "personal"),
            (&["work", "personal"][..], Some("work"), "work"),
        ] {
            let temp = tempdir()?;
            let _guard = setup_test_home(temp.path());
            for profile in existing {
                get_profile_dir(profile)?;
            }
            if let Some(name) = configured {
                super::super::config::update_config(|config| {
                    config.default_profile = name.to_string();
                })?;
            }
            assert_eq!(Storage::new_unwatched("")?.profile(), expected);
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn workspace_ordering_round_trips_and_serializes_updates() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        assert!(load_workspace_ordering()?.order.is_empty());
        let path = workspace_ordering_path()?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, "   ")?;
        assert!(load_workspace_ordering()?.order.is_empty());

        let order = |items: &[&str]| WorkspaceOrdering {
            order: items.iter().map(|s| s.to_string()).collect(),
        };
        save_workspace_ordering(&order(&["/repo/a::main", "/repo/c::__session__::abc123"]))?;
        save_workspace_ordering(&order(&["b"]))?;
        assert_eq!(load_workspace_ordering()?.order, ["b"]);

        std::thread::scope(|scope| {
            for tid in 0..16 {
                scope.spawn(move || {
                    update_workspace_ordering(|ord| {
                        ord.order.push(format!("ws-{tid}"));
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let loaded = load_workspace_ordering()?;
        assert_eq!(loaded.order.len(), 17);
        assert!((0..16).all(|tid| loaded.order.contains(&format!("ws-{tid}"))));
        Ok(())
    }

    #[test]
    #[serial]
    fn update_error_leaves_both_files_untouched() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-update-err")?;
        storage.update(|i, g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            g.push(Group::new("seed-group", "work/seed"));
            Ok(())
        })?;
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let before = (fs::read(&storage.sessions_path)?, fs::read(&groups_path)?);

        let outcome: Result<()> = storage.update(|instances, groups| {
            instances.push(Instance::new("doomed", "/tmp/doomed"));
            groups.push(Group::new("doomed-group", "doomed/path"));
            Err(anyhow!("forced abort"))
        });
        assert!(outcome.is_err());
        assert_eq!(
            (fs::read(&storage.sessions_path)?, fs::read(&groups_path)?),
            before
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn update_serializes_concurrent_writers_same_profile() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-update-concurrent")?;
        let other = Storage::new_unwatched("test-update-concurrent")?;
        assert!(Arc::ptr_eq(&storage.save_lock, &other.save_lock));
        assert!(!Arc::ptr_eq(
            &storage.save_lock,
            &Storage::new_unwatched("test-registry-distinct")?.save_lock
        ));

        std::thread::scope(|scope| {
            for tid in 0..32 {
                scope.spawn(move || {
                    Storage::new_unwatched("test-update-concurrent")
                        .unwrap()
                        .update(|instances, _| {
                            instances.push(Instance::new(&format!("inst-{tid}"), "/tmp/inst"));
                            Ok(())
                        })
                        .unwrap();
                });
            }
        });
        let titles: Vec<_> = storage.load()?.into_iter().map(|i| i.title).collect();
        assert_eq!(titles.len(), 32, "lost updates");
        assert!((0..32).all(|tid| titles.contains(&format!("inst-{tid}"))));
        Ok(())
    }

    #[test]
    #[serial]
    fn instance_lifecycle_lock_serializes_same_profile_and_instance() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let profile = "test-instance-lifecycle-lock";
        let instance_id = Instance::new("locked", "/tmp/locked").id;
        let storage = Storage::new_unwatched(profile)?;
        let first = storage.acquire_instance_lifecycle_lock(&instance_id)?;
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _observer = observe_lock_contention_for_test(contended_tx);
                let peer = Storage::new_unwatched(profile).unwrap();
                let _second = peer.acquire_instance_lifecycle_lock(&instance_id).unwrap();
                acquired_tx.send(()).unwrap();
            });
            let contended = contended_rx.recv_timeout(Duration::from_secs(2));
            let entered_early = acquired_rx.try_recv().is_ok();
            drop(first);
            assert!(contended.is_ok(), "peer never demonstrated contention");
            assert!(!entered_early, "peer acquired before release");
            acquired_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("peer did not acquire lifecycle lock after release");
        });
        assert!(storage
            .acquire_instance_lifecycle_lock("../escape")
            .is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_does_not_serialize_across_profiles() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage_a = Storage::new_unwatched("test-update-profile-a")?;
        let storage_b = Storage::new_unwatched("test-update-profile-b")?;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (overlap_tx, overlap_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let storage_a = &storage_a;
            let storage_b = &storage_b;
            let a = scope.spawn(move || {
                storage_a.update(|instances, _| {
                    entered_tx.send(()).unwrap();
                    let overlap = overlap_rx.recv_timeout(Duration::from_secs(2));
                    instances.push(Instance::new("a1", "/tmp/a1"));
                    overlap.context("profile B must enter while profile A owns its update locks")
                })
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let b = scope.spawn(move || {
                storage_b.update(|instances, _| {
                    let _ = overlap_tx.send(());
                    instances.push(Instance::new("b1", "/tmp/b1"));
                    Ok(())
                })
            });
            a.join().unwrap()?;
            b.join().unwrap()?;
            Ok::<_, anyhow::Error>(())
        })?;
        assert_eq!(storage_a.load()?[0].title, "a1");
        assert_eq!(storage_b.load()?[0].title, "b1");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_takes_same_lock_across_threads() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-commit-lock")?;
        for layer in ["mutex", "flock"] {
            let mutex = (layer == "mutex").then(|| storage.save_lock.lock().unwrap());
            let flock = (layer == "flock")
                .then(|| {
                    acquire_storage_flock(
                        storage.sessions_path.parent().unwrap(),
                        STORAGE_LOCK_FILENAME,
                    )
                })
                .transpose()?;
            let (contended_tx, contended_rx) = std::sync::mpsc::channel();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let writer = std::thread::spawn(move || {
                let _observer = observe_lock_contention_for_test(contended_tx);
                Storage::new_unwatched("test-commit-lock")
                    .unwrap()
                    .update(|instances, _| {
                        entered_tx.send(()).unwrap();
                        *instances = vec![Instance::new(layer, "/tmp/committed")];
                        Ok(())
                    })
                    .unwrap();
            });
            let contended = contended_rx.recv_timeout(Duration::from_secs(2));
            let entered_early = entered_rx.try_recv().is_ok();
            drop(mutex);
            drop(flock);
            writer.join().unwrap();
            assert!(
                contended.is_ok(),
                "{layer}: writer never demonstrated contention"
            );
            assert!(
                !entered_early,
                "{layer}: mutation entered before lock release"
            );
            assert_eq!(storage.load()?[0].title, layer);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn test_update_write_failure_emits_no_notify() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        if running_as_root() {
            eprintln!(
                "test_update_write_failure_emits_no_notify: skipping (running as root; \
                 uid 0 bypasses the read-only dir bit, so the write cannot be made to fail)"
            );
            return Ok(());
        }

        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let _watch_env = crate::session::test_support::EnvGuard::unset(&["AOE_FILE_WATCH"]);
        let svc = FileWatchService::new().expect("live svc");
        let storage = Storage::new("test-update-no-notify", svc.clone())?;
        storage.update(|instances, _groups| {
            *instances = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })?;

        let profile_dir = get_profile_dir("test-update-no-notify")?;
        let sessions_path = profile_dir.join("sessions.json");
        let groups_path = profile_dir.join("groups.json");
        let (mut sessions_rx, _sessions_h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir.clone(),
                    matcher: FileMatcher::Exact(sessions_path),
                    debounce: None,
                },
                128,
            )
            .expect("subscribe sessions");
        let (mut groups_rx, _groups_h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir.clone(),
                    matcher: FileMatcher::Exact(groups_path),
                    debounce: None,
                },
                128,
            )
            .expect("subscribe groups");

        storage.update(|instances, groups| {
            instances.push(Instance::new("published", "/tmp/published"));
            groups.push(Group::new("published", "published"));
            Ok(())
        })?;
        tokio::time::timeout(
            Duration::from_secs(2),
            crate::file_watch::test_support::dispatch_barrier(&svc),
        )
        .await?;
        for rx in [&mut sessions_rx, &mut groups_rx] {
            let mut local = false;
            loop {
                match rx.try_recv() {
                    Ok(event) => local |= event.source == crate::file_watch::EventSource::Local,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        panic!("live dispatcher closed")
                    }
                }
            }
            assert!(
                local,
                "successful write must positively witness local delivery"
            );
        }

        let original_mode = fs::metadata(&profile_dir)?.permissions().mode();
        let mut readonly = fs::metadata(&profile_dir)?.permissions();
        readonly.set_mode(0o500);
        fs::set_permissions(&profile_dir, readonly)?;

        let update_res = storage.update(|instances, groups| {
            instances.push(Instance::new("late", "/tmp/late"));
            groups.push(Group::new("late-group", "/tmp/late-group"));
            Ok(())
        });

        let mut restore = fs::metadata(&profile_dir)?.permissions();
        restore.set_mode(original_mode);
        fs::set_permissions(&profile_dir, restore)?;

        assert!(update_res.is_err(), "write failure must surface as Err");

        tokio::time::timeout(
            Duration::from_secs(2),
            crate::file_watch::test_support::dispatch_barrier(&svc),
        )
        .await?;
        for rx in [&mut sessions_rx, &mut groups_rx] {
            loop {
                match rx.try_recv() {
                    Ok(event) => assert_ne!(
                        event.source,
                        crate::file_watch::EventSource::Local,
                        "failed write emitted a successful-write notification"
                    ),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        panic!("live dispatcher closed")
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn update_rewrites_groups_only_when_changed() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-skip-groups-write")?;
        storage.update(|i, g| {
            *i = vec![Instance::new("seed", "/tmp/seed")];
            g.push(Group::new("seed-group", "seed-group"));
            Ok(())
        })?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let sentinel = std::time::UNIX_EPOCH + Duration::from_secs(946_684_800);
        fs::File::options()
            .write(true)
            .open(&groups_path)?
            .set_times(fs::FileTimes::new().set_modified(sentinel))?;
        storage.update(|instances, _groups| {
            instances.push(Instance::new("added", "/tmp/added"));
            Ok(())
        })?;
        assert_eq!(fs::metadata(&groups_path)?.modified()?, sentinel);

        storage.update(|_instances, groups| {
            groups.push(Group::new("new-group", "work/new-group"));
            Ok(())
        })?;
        let (_, groups) = storage.load_with_groups()?;
        let paths: Vec<_> = groups.iter().map(|group| group.path.as_str()).collect();
        assert_eq!(paths, ["seed-group", "work/new-group"]);
        Ok(())
    }

    #[test]
    #[serial]
    fn test_save_lock_registry_recovers_from_poison() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage_outer = Storage::new_unwatched("test-poison-recovery")?;
        let _ = std::thread::spawn(move || {
            let _ = storage_outer.update(|_instances, _groups| -> Result<()> {
                panic!("forced poison");
            });
        })
        .join();

        let storage_after = Storage::new_unwatched("test-poison-recovery")?;
        storage_after.update(|instances, _groups| {
            instances.push(Instance::new("after-poison", "/tmp/after"));
            Ok(())
        })?;

        let loaded = storage_after.load()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "after-poison");
        Ok(())
    }

    #[test]
    fn profile_batch_move_rejects_collision_and_merges_fresh_source() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        std::fs::create_dir_all(&source_dir)?;
        std::fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("move-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("move-target", target_dir.join("sessions.json"));
        let mut first = Instance::new("first", "/repo/first");
        first.source_profile = "move-source".to_string();
        let mut second = Instance::new("second", "/repo/second");
        second.source_profile = "move-source".to_string();
        source.update(|instances, _groups| {
            *instances = vec![first.clone(), second.clone()];
            Ok(())
        })?;
        let owner = Instance::new("second", "/repo/second/");
        target.update(|instances, _groups| {
            instances.push(owner);
            Ok(())
        })?;

        let mut first_after = first.clone();
        first_after.group_path = "moved".to_string();
        let mut second_after = second.clone();
        second_after.group_path = "moved".to_string();
        let changes = [
            (first.clone(), first_after.clone()),
            (second.clone(), second_after.clone()),
        ];
        let group_move = GroupMovePlan::subtree("", "moved");
        let rejected =
            source.move_instances_to(&target, &changes, &group_move, |existing, candidates| {
                if candidates.iter().any(|candidate| {
                    existing.iter().any(|row| {
                        row.title == candidate.title
                            && row.project_path.trim_end_matches('/')
                                == candidate.project_path.trim_end_matches('/')
                    })
                }) {
                    return Err(anyhow!("duplicate"));
                }
                Ok(())
            });
        assert!(rejected.is_err());
        assert_eq!(source.load()?.len(), 2);
        assert_eq!(target.load()?.len(), 1);

        target.update(|instances, _groups| {
            instances.clear();
            Ok(())
        })?;
        source.update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|instance| instance.id == first.id)
                .unwrap()
                .unread = true;
            Ok(())
        })?;
        let moved = source.move_instances_to(
            &target,
            &changes,
            &group_move,
            |_existing, _candidates| Ok(()),
        )?;
        assert_eq!(moved.len(), 2);
        let moved_first = moved
            .iter()
            .find(|instance| instance.id == first.id)
            .unwrap();
        assert!(moved_first.unread, "fresh peer field must survive");
        assert_eq!(moved_first.group_path, "moved");
        assert!(source.load()?.is_empty());
        assert_eq!(target.load()?.len(), 2);
        Ok(())
    }

    #[test]
    fn profile_move_runs_external_effect_only_after_locked_target_validation() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-effect");
        let target_dir = temp.path().join("target-effect");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("effect-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("effect-target", target_dir.join("sessions.json"));
        let before = Instance::new("collision", "/repo/collision");
        source.update(|instances, _groups| {
            instances.push(before.clone());
            Ok(())
        })?;
        target.update(|instances, _groups| {
            instances.push(Instance::new("collision", "/repo/collision/"));
            Ok(())
        })?;
        let effect_ran = std::cell::Cell::new(false);

        let result = source.move_instance_to_with_effect(
            &target,
            &before,
            &before,
            false,
            |instances, candidate| {
                if instances.iter().any(|row| {
                    row.title == candidate.title
                        && row.project_path.trim_end_matches('/')
                            == candidate.project_path.trim_end_matches('/')
                }) {
                    return Err(anyhow!("duplicate"));
                }
                Ok(())
            },
            |_| {
                effect_ran.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!effect_ran.get());
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        Ok(())
    }

    #[test]
    fn profile_move_transfers_explicit_empty_group_metadata() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-empty-group");
        let target_dir = temp.path().join("target-empty-group");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("empty-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("empty-target", target_dir.join("sessions.json"));
        source.update(|_instances, groups| {
            let mut group = Group::new("empty", "empty");
            group.collapsed = true;
            group.archived_at = Some(chrono::Utc::now());
            groups.push(group);
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        let moved = source.move_instances_to(
            &target,
            &[],
            &GroupMovePlan::subtree("empty", "renamed"),
            |_existing, candidates| {
                assert!(candidates.is_empty());
                Ok(())
            },
        )?;

        assert!(moved.is_empty());
        assert!(source
            .load_with_groups()?
            .1
            .iter()
            .all(|group| group.path != "empty"));
        let target_group = target
            .load_with_groups()?
            .1
            .into_iter()
            .find(|group| group.path == "renamed")
            .expect("explicit empty group metadata transferred");
        assert!(target_group.collapsed);
        assert!(target_group.archived_at.is_some());
        Ok(())
    }

    #[test]
    fn profile_group_move_rejects_fresh_unplanned_member() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-members");
        let target_dir = temp.path().join("target-members");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("member-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("member-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("snapshot", "/repo/snapshot");
        before.group_path = "team".to_string();
        let mut after = before.clone();
        after.group_path = "moved".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("team", "team"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        source.update(|instances, _groups| {
            let mut concurrent = Instance::new("concurrent", "/repo/concurrent");
            concurrent.group_path = "team/new".to_string();
            instances.push(concurrent);
            Ok(())
        })?;
        let error = source
            .move_instances_to(
                &target,
                &[(before, after)],
                &GroupMovePlan::subtree("team", "moved"),
                |_existing, _candidates| Ok(()),
            )
            .expect_err("fresh subtree membership must be revalidated under lock");

        assert!(error.to_string().contains("group membership changed"));
        let (source_rows, source_groups) = source.load_with_groups()?;
        assert_eq!(source_rows.len(), 2);
        assert!(source_groups.iter().any(|group| group.path == "team"));
        assert!(target.load()?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn profile_move_syncs_resolved_symlink_target_parent() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let source_dir = temp.path().join("source-symlink");
        let target_dir = temp.path().join("target-symlink");
        let resolved_sessions_dir = temp.path().join("resolved-sessions");
        let resolved_groups_dir = temp.path().join("resolved-groups");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        fs::create_dir_all(&resolved_sessions_dir)?;
        fs::create_dir_all(&resolved_groups_dir)?;
        let source = Storage::new_for_test_path("symlink-source", source_dir.join("sessions.json"));
        let target_link = target_dir.join("sessions.json");
        let target_groups_link = target_dir.join("groups.json");
        let resolved_sessions = resolved_sessions_dir.join("sessions.json");
        let resolved_groups = resolved_groups_dir.join("groups.json");
        fs::write(&resolved_sessions, b"[]")?;
        fs::write(&resolved_groups, b"[]")?;
        symlink(&resolved_sessions, &target_link)?;
        symlink(&resolved_groups, &target_groups_link)?;
        let target = Storage::new_for_test_path("symlink-target", target_link);
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "symlink-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;

        let result = source.move_instances_to_inner(
            &target,
            &[(before.clone(), before.clone())],
            MoveTransactionPlan {
                group_move: &GroupMovePlan::single("work", "work"),
                merge_complete_post: true,
                account_swap: false,
            },
            |_existing, _candidates| Ok(()),
            |_| Ok(()),
            {
                let mut synced = Vec::new();
                move |path| {
                    synced.push(path.to_path_buf());
                    if path == resolved_sessions {
                        assert_eq!(
                            synced,
                            vec![resolved_groups.clone(), resolved_sessions.clone()]
                        );
                        Err(anyhow!("forced resolved-directory sync failure"))
                    } else {
                        assert_eq!(path, resolved_groups);
                        Ok(())
                    }
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        Ok(())
    }

    #[test]
    #[serial]
    fn profile_move_keeps_both_rows_when_target_directory_sync_fails() -> Result<()> {
        for effect_runs in [false, true] {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env("sync")?;
            let effect_ran = std::cell::Cell::new(false);
            let result = source.move_instances_to_inner(
                &target,
                &[(before.clone(), before.clone())],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "work"),
                    merge_complete_post: true,
                    account_swap: false,
                },
                |_existing, _candidates| Ok(()),
                |_moved| {
                    effect_ran.set(effect_runs);
                    Ok(())
                },
                |_path| Err(anyhow!("forced target directory sync failure")),
            );
            assert!(result.is_err());
            assert_eq!(effect_ran.get(), effect_runs);
            for storage in [&source, &target] {
                let (rows, groups) = storage.load_with_groups()?;
                assert_eq!(
                    rows.len(),
                    1,
                    "{}: row retained for recovery",
                    storage.profile()
                );
                assert!(groups.iter().any(|group| group.path == "work"));
            }
        }
        Ok(())
    }

    #[test]
    fn apply_group_move_is_byte_stable_without_semantic_change() -> Result<()> {
        let mut mover = Instance::new("mover", "/repo/mover");
        mover.group_path = "work".to_string();
        let mut stayer = Instance::new("stayer", "/repo/stayer");
        stayer.group_path = "work".to_string();

        let source_instances = vec![stayer];
        let mut work_group = Group::new("work", "work");
        work_group.collapsed = true;
        work_group.archived_at = Some(chrono::Utc::now());
        let mut source_groups = vec![work_group];
        let target_instances = vec![mover];
        let mut target_groups = Vec::new();

        let before = serde_json::to_vec_pretty(&source_groups)?;
        apply_group_move(
            &GroupMovePlan::single("work", "work"),
            &source_instances,
            &mut source_groups,
            &target_instances,
            &mut target_groups,
        );
        let after = serde_json::to_vec_pretty(&source_groups)?;
        assert_eq!(
            before, after,
            "source groups must be byte-stable, including collapsed/archived metadata"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn profile_move_rejects_shared_storage_lock_inode() -> Result<()> {
        let (temp, _guard, source, target, before, _after) = setup_recovery_env("inode")?;
        let source_dir = temp.path().join("inode-source");
        let target_dir = temp.path().join("inode-target");
        target.update(|_instances, groups| {
            groups.push(Group::new("target", "target"));
            Ok(())
        })?;
        let target_lock = target_dir.join(STORAGE_LOCK_FILENAME);
        fs::remove_file(&target_lock)?;
        fs::hard_link(source_dir.join(STORAGE_LOCK_FILENAME), &target_lock)?;
        let effect_ran = std::cell::Cell::new(false);
        let try_move = || {
            let effect = |_: &Instance| {
                effect_ran.set(true);
                Ok(())
            };
            source
                .move_instance_to_with_effect(
                    &target,
                    &before,
                    &before,
                    false,
                    |_, _| Ok(()),
                    effect,
                )
                .expect_err("shared inode must be rejected before locking or effects")
                .to_string()
        };

        assert!(try_move().contains("physical storage lock"));
        assert_eq!(source.load()?.len(), 1);
        assert!(target.load()?.is_empty());

        fs::remove_file(&target_lock)?;
        fs::File::create(&target_lock)?;
        let target_groups = target_dir.join("groups.json");
        fs::remove_file(&target_groups)?;
        fs::hard_link(source_dir.join("groups.json"), &target_groups)?;
        assert!(try_move().contains("physical groups file"));
        assert!(!effect_ran.get());
        Ok(())
    }

    #[test]
    fn recent_project_entry_for_cases() {
        let mut inst = Instance::new("s", "/home/me/projects/frontend/");
        inst.tool = "claude".to_string();
        let accessed = inst.created_at + chrono::Duration::hours(5);
        inst.last_accessed_at = Some(accessed);
        let e = recent_project_entry_for(&inst).expect("single-repo session recorded");
        assert_eq!(
            (&*e.path, &*e.display_name, &*e.tool),
            ("/home/me/projects/frontend", "frontend", "claude")
        );
        assert_eq!(e.last_used_at, accessed.to_rfc3339());

        inst.scratch = true;
        assert!(recent_project_entry_for(&inst).is_none());
    }

    #[test]
    #[serial]
    fn record_recent_project_upserts_sorts_and_caps() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        for i in 0..(RECENT_PROJECTS_CAP + 5) {
            record_recent_project(RecentProjectEntry {
                path: format!("/p/{i}"),
                display_name: format!("{i}"),
                tool: "claude".to_string(),
                last_used_at: format!("2026-06-15T00:{:02}:00+00:00", i),
            })?;
        }
        let loaded = load_recent_projects()?;
        assert_eq!(loaded.len(), RECENT_PROJECTS_CAP, "capped");
        assert_eq!(loaded[0].path, format!("/p/{}", RECENT_PROJECTS_CAP + 4));
        assert!(loaded.iter().all(|p| p.path != "/p/0"));

        record_recent_project(RecentProjectEntry {
            path: format!("/p/{}", RECENT_PROJECTS_CAP + 1),
            display_name: "x".to_string(),
            tool: "claude".to_string(),
            last_used_at: "2026-06-15T23:59:00+00:00".to_string(),
        })?;
        let loaded = load_recent_projects()?;
        assert_eq!(
            loaded.len(),
            RECENT_PROJECTS_CAP,
            "still capped after upsert"
        );
        assert_eq!(loaded[0].path, format!("/p/{}", RECENT_PROJECTS_CAP + 1));
        assert_eq!(
            loaded
                .iter()
                .filter(|p| p.path == format!("/p/{}", RECENT_PROJECTS_CAP + 1))
                .count(),
            1,
            "no duplicate entry"
        );
        Ok(())
    }
    #[test]
    #[serial]
    fn profile_move_crash_after_target_publication_leaves_duplicate_id() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("repro")?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = source.move_instances_to_inner(
                &target,
                &[(before.clone(), before.clone())],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                    account_swap: false,
                },
                |_existing, _candidates| Ok(()),
                |_| Ok(()),
                |_path| panic!("simulated crash after target publication"),
            );
        }));
        assert!(result.is_err(), "the simulated crash must abort the move");
        let (source_rows, target_rows) = (source.load()?, target.load()?);
        assert_eq!((source_rows.len(), target_rows.len()), (1, 1));
        assert_eq!(
            source_rows[0].id, target_rows[0].id,
            "ambiguous duplicate id"
        );
        Ok(())
    }

    fn setup_recovery_env(
        tag: &str,
    ) -> Result<(
        tempfile::TempDir,
        AppDirGuard,
        Storage,
        Storage,
        Instance,
        Instance,
    )> {
        let temp = tempfile::TempDir::new()?;
        let guard = isolate_app_dir_at(temp.path());
        let source_dir = temp.path().join(format!("{tag}-source"));
        let target_dir = temp.path().join(format!("{tag}-target"));
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source =
            Storage::new_for_test_path(&format!("{tag}-source"), source_dir.join("sessions.json"));
        let target =
            Storage::new_for_test_path(&format!("{tag}-target"), target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = format!("{tag}-source");
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;
        let mut after = before.clone();
        after.group_path = "moved".to_string();
        Ok((temp, guard, source, target, before, after))
    }

    fn run_crashing_move(source: &Storage, target: &Storage, point: &'static str) {
        let _crash = ArmedCrashPoint::arm(point);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut after = source.load().unwrap().remove(0);
            after.group_path = "moved".to_string();
            let before = source.load().unwrap().remove(0);
            let _ = source.move_instances_to_inner(
                target,
                &[(before, after)],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                    account_swap: false,
                },
                |_existing, _candidates| Ok(()),
                |_| Ok(()),
                sync_resolved_parent_directory,
            );
        }));
    }

    #[test]
    #[serial]
    fn profile_move_crash_recovery_target_wins_from_each_crash_point() -> Result<()> {
        for point in [
            "profile-move-target",
            "profile-move-source-groups",
            "profile-move-source-sessions",
        ] {
            let (_temp, _guard, source, target, _before, _after) =
                setup_recovery_env(point.replace('-', "_").as_str())?;
            run_crashing_move(&source, &target, point);

            assert_eq!(source.load()?.len(), 1, "{point}: source row remains");
            assert_eq!(target.load()?.len(), 1, "{point}: target copy durable");
            assert_eq!(
                journal_entry_count(&source),
                1,
                "{point}: exactly one journal entry guards the residual"
            );

            let outcome = reconcile_loaded(&[&source, &target]);

            assert!(outcome.repaired, "{point}: repair must run");
            assert!(
                outcome.reports.is_empty(),
                "{point}: no legacy ambiguity may remain: {reports:?}",
                reports = outcome
                    .reports
                    .iter()
                    .map(|r| r.actionable_message())
                    .collect::<Vec<_>>()
            );
            assert!(source.load()?.is_empty(), "{point}: losing source emptied");
            let target_rows = target.load()?;
            assert_eq!(target_rows.len(), 1, "{point}");
            assert_eq!(target_rows[0].group_path, "moved", "{point}");
            let target_groups = target.load_with_groups()?.1;
            assert!(
                target_groups.iter().any(|group| group.path == "moved"),
                "{point}: winning sidecar keeps the moved group"
            );
            let source_groups = source.load_with_groups()?.1;
            assert!(
                !source_groups.iter().any(|group| group.path == "work"),
                "{point}: losing attributable group entry pruned"
            );
            assert_eq!(journal_entry_count(&source), 0, "{point}: journal consumed");
            let backups = fs::read_dir(source.sessions_path().parent().unwrap())?
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".pre-recovery-")
                })
                .count();
            assert!(
                backups >= 1,
                "{point}: sessions.json backed up before repair"
            );

            let outcome = reconcile_loaded(&[&source, &target]);
            assert!(!outcome.repaired && outcome.reports.is_empty(), "{point}");
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn profile_move_crash_before_publication_source_wins() -> Result<()> {
        let (temp, _guard, source, target, _before, _after) = setup_recovery_env("prepub")?;
        assert!(
            crate::session::get_app_dir()?.starts_with(temp.path()),
            "identity/title locks must stay below the fixture temp root"
        );
        source.update(|_instances, groups| {
            let mut bystander = Group::new("moved", "moved");
            bystander.collapsed = true;
            groups.push(bystander);
            Ok(())
        })?;
        target.update(|_instances, groups| {
            let mut existing = Group::new("moved", "moved");
            existing.collapsed = true;
            groups.push(existing);
            Ok(())
        })?;
        let source_groups_before = source.load_with_groups()?.1;
        let target_groups_before = target.load_with_groups()?.1;
        run_crashing_move(&source, &target, "profile-move-journal");

        assert_eq!(source.load()?.len(), 1, "source row untouched");
        assert!(target.load()?.is_empty(), "target never published");
        assert_eq!(journal_entry_count(&source), 1);

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(outcome.repaired, "the leaked journal must be consumed");
        assert!(outcome.reports.is_empty());
        assert_eq!(source.load()?.len(), 1, "source wins, nothing removed");
        assert!(target.load()?.is_empty());
        assert_eq!(source.load_with_groups()?.1, source_groups_before);
        assert_eq!(target.load_with_groups()?.1, target_groups_before);
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn legacy_duplicate_without_journal_is_surfaced_never_arbitrated() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("legacy")?;
        let id = before.id.clone();
        push_copy(&target, &before)?;

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(!outcome.repaired);
        assert_eq!(
            outcome.reports.len(),
            1,
            "exactly the duplicated id surfaces"
        );
        let report = &outcome.reports[0];
        assert_eq!(report.id, id);
        assert_eq!(report.copies.len(), 2);
        let message = report.actionable_message();
        assert!(message.contains(&id), "message names the session id");
        assert!(
            message.contains("sessions.json"),
            "message names store files"
        );
        for storage in [&source, &target] {
            assert!(
                message.contains(storage.profile()),
                "message names profile {}: {message}",
                storage.profile()
            );
            assert_eq!(storage.load()?.len(), 1, "no automatic arbitration");
        }
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn insufficient_evidence_journal_is_surfaced_never_consumed() -> Result<()> {
        let week_ago_ms = now_ms() - 8 * 24 * 3600 * 1000;
        let cases = [
            (
                "insuff-wrong-version",
                move_journal::MOVE_JOURNAL_VERSION + 1,
                0,
            ),
            (
                "insuff-expired",
                move_journal::MOVE_JOURNAL_VERSION,
                week_ago_ms,
            ),
        ];
        for (tag, version, created_at) in cases {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(tag)?;
            push_copy(&target, &before)?;
            let entry = move_journal::MoveJournalEntry {
                version,
                created_at_epoch_ms: created_at,
                ..fresh_journal_entry(&source, &target, &before.id)
            };
            move_journal::record(&entry, source.sessions_path())?;
            assert_eq!(journal_entry_count(&source), 1, "{tag}");

            let outcome = reconcile_loaded(&[&source, &target]);

            assert!(
                !outcome.repaired,
                "{tag}: insufficient evidence must not arbitrate"
            );
            assert_eq!(outcome.reports.len(), 1, "{tag}: duplicate stays surfaced");
            assert_eq!(source.load()?.len(), 1, "{tag}");
            assert_eq!(target.load()?.len(), 1, "{tag}");
            assert_eq!(
                journal_entry_count(&source),
                1,
                "{tag}: entry stays on disk"
            );
        }
        Ok(())
    }

    #[test]
    fn resolve_miss_is_transient_not_permanent() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("resolvemiss")?;
        push_copy(&target, &before)?;
        move_journal::record(
            &fresh_journal_entry(&source, &target, &before.id),
            source.sessions_path(),
        )?;

        let outcome = reconcile_loaded(&[&source]);
        assert!(!outcome.repaired, "nothing to arbitrate without the target");
        assert_eq!(
            journal_entry_count(&source),
            1,
            "entry must survive the miss"
        );

        let outcome = reconcile_loaded(&[&source, &target]);
        assert!(
            outcome.repaired,
            "resolve-miss must not poison later passes"
        );
        assert!(source.load()?.is_empty());
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn multi_id_batch_arbitrates_surviving_ids_and_skips_vanished_ones() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("multi")?;
        let vanished_id = Instance::new("vanished", "/repo/vanished").id;
        push_copy(&target, &before)?;
        let mut entry = fresh_journal_entry(&source, &target, &before.id);
        entry.ids.push(vanished_id.clone());
        entry.ids.sort();
        move_journal::record(&entry, source.sessions_path())?;

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(outcome.repaired, "the duplicated sibling must arbitrate");
        assert!(outcome.reports.is_empty());
        assert!(
            !source.load()?.iter().any(|row| row.id == before.id),
            "loser copy removed"
        );
        assert_eq!(target.load()?.len(), 1, "winner kept");
        assert!(
            !journal_entry_scan_ids(&source)
                .iter()
                .any(|id| id == &before.id),
            "journal consumed despite the vanished sibling"
        );
        Ok(())
    }

    #[test]
    fn post_journal_store_edits_degrade_to_legacy() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("edited")?;
        push_copy(&target, &before)?;
        let mut entry = fresh_journal_entry(&source, &target, &before.id);
        entry.created_at_epoch_ms = now_ms() - 10 * 60 * 1000;
        move_journal::record(&entry, source.sessions_path())?;

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(
            !outcome.repaired,
            "post-journal edits must block arbitration"
        );
        assert_eq!(outcome.reports.len(), 1);
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        assert_eq!(journal_entry_count(&source), 1);
        let journal_path = first_journal_path(&source);
        assert!(
            unusable_journal_entries_contains(&journal_path),
            "mtime-degraded entry is blacklisted like other permanent causes"
        );
        Ok(())
    }

    #[test]
    fn duplicate_ids_and_aliased_endpoints_are_rejected_before_locks() -> Result<()> {
        for case in ["duplicate-ids", "aliased-endpoints"] {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(case)?;
            let mut entry = fresh_journal_entry(&source, &target, &before.id);
            let storages: &[&Storage] = if case == "duplicate-ids" {
                entry.ids.push(before.id.clone());
                &[&source, &target]
            } else {
                entry.target_profile = source.profile().to_string();
                entry.target_sessions_path = source.sessions_path().to_path_buf();
                &[&source]
            };
            let journal_path = move_journal::record(&entry, source.sessions_path())?;
            let outcome = reconcile_loaded(storages);

            assert!(!outcome.repaired, "{case}");
            assert_eq!(journal_entry_count(&source), 1, "{case}: evidence remains");
            assert!(
                unusable_journal_entries_contains(&journal_path),
                "{case}: semantic invalidity is permanently recorded"
            );
        }
        Ok(())
    }

    #[test]
    fn dual_storage_lock_blocks_target_only_writer() -> Result<()> {
        let (_temp, _guard, source, target, _before, _after) = setup_recovery_env("dual-lock")?;
        let target_path = target.sessions_path().to_path_buf();
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut writer = None;

        let (contended, entered_early) = with_two_storage_locks(&source, &target, || {
            writer = Some(std::thread::spawn(move || {
                let target = Storage::new_for_test_path("dual-lock-target", target_path);
                let _observer = observe_lock_contention_for_test(contended_tx);
                target
                    .update(|instances, _| {
                        instances.clear();
                        Ok(())
                    })
                    .unwrap();
                done_tx.send(()).unwrap();
            }));
            let contended = contended_rx.recv_timeout(Duration::from_secs(2));
            let entered_early = done_rx.try_recv().is_ok();
            Ok((contended, entered_early))
        })?;

        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("target writer proceeds after dual lock release");
        writer.unwrap().join().unwrap();
        assert!(
            contended.is_ok(),
            "target writer never demonstrated lock contention"
        );
        assert!(
            !entered_early,
            "target writer entered before dual lock release"
        );
        Ok(())
    }

    #[test]
    fn unresolved_newer_intent_blocks_older_overlapping_journal() -> Result<()> {
        for case in ["resolve-miss", "opaque"] {
            let (_temp, _guard, a, b, before, _after) = setup_recovery_env(case)?;
            push_copy(&b, &before)?;
            let now = now_ms();
            let mut older = fresh_journal_entry(&a, &b, &before.id);
            older.created_at_epoch_ms = now - 60_000;
            move_journal::record(&older, a.sessions_path())?;
            if case == "resolve-miss" {
                let mut newer = fresh_journal_entry(&b, &a, &before.id);
                newer.created_at_epoch_ms = now;
                newer.target_profile = "missing-profile".to_string();
                newer.target_sessions_path = a.sessions_path().with_file_name("missing.json");
                move_journal::record(&newer, b.sessions_path())?;
            } else {
                let journal_dir = b.sessions_path().parent().unwrap().join(".move-journal");
                fs::create_dir_all(&journal_dir)?;
                fs::write(
                    journal_dir.join("move-99999999999999999999-1.json"),
                    b"not-json",
                )?;
            }
            let outcome = reconcile_loaded(&[&a, &b]);

            assert!(!outcome.repaired, "{case}: older intent must not apply");
            assert_eq!(outcome.reports.len(), 1, "{case}: duplicate stays surfaced");
            assert_eq!(a.load()?.len(), 1, "{case}: newer target copy remains");
            assert_eq!(b.load()?.len(), 1, "{case}: no copy is removed");
        }
        Ok(())
    }

    #[test]
    fn shadowed_batch_propagates_block_to_every_id() -> Result<()> {
        let (_temp, _guard, a, b, x, _after) = setup_recovery_env("transitive")?;
        let mut y = Instance::new("y", "/repo/y");
        y.source_profile = a.profile().to_string();
        a.update(|instances, _| {
            instances.push(y.clone());
            Ok(())
        })?;
        b.update(|instances, _| {
            let mut x_copy = x.clone();
            x_copy.source_profile = b.profile().to_string();
            let mut y_copy = y.clone();
            y_copy.source_profile = b.profile().to_string();
            instances.extend([x_copy, y_copy]);
            Ok(())
        })?;
        let now = now_ms();

        let mut j1 = fresh_journal_entry(&a, &b, &y.id);
        j1.created_at_epoch_ms = now - 120_000;
        let mut j2 = fresh_journal_entry(&b, &a, &x.id);
        j2.ids.push(y.id.clone());
        j2.ids.sort();
        j2.created_at_epoch_ms = now - 60_000;
        let mut j3 = fresh_journal_entry(&a, &b, &x.id);
        j3.target_profile = "missing".to_string();
        j3.target_sessions_path = a.sessions_path().with_file_name("missing.json");
        j3.created_at_epoch_ms = now;
        move_journal::record(&j1, a.sessions_path())?;
        move_journal::record(&j2, b.sessions_path())?;
        move_journal::record(&j3, a.sessions_path())?;
        let outcome = reconcile_loaded(&[&a, &b]);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 2);
        assert_eq!(a.load()?.len(), 2, "newer X+Y target remains intact");
        assert_eq!(b.load()?.len(), 2, "no stale journal deletes either copy");
        Ok(())
    }

    #[test]
    fn duplicate_target_rows_never_become_an_automatic_winner() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("target-dup")?;
        target.update(|instances, _| {
            for _ in 0..2 {
                let mut copy = before.clone();
                copy.source_profile = target.profile().to_string();
                instances.push(copy);
            }
            Ok(())
        })?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        move_journal::record(&entry, source.sessions_path())?;
        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 1);
        assert_eq!(source.load()?.len(), 1, "source copy is preserved");
        assert_eq!(
            target.load()?.len(),
            2,
            "ambiguous target copies remain surfaced"
        );
        assert_eq!(
            journal_entry_count(&source),
            1,
            "evidence remains for manual resolution"
        );
        Ok(())
    }

    #[test]
    fn invalid_id_entry_is_permanently_insufficient() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("badid")?;
        push_copy(&target, &before)?;
        let entry = move_journal::MoveJournalEntry {
            ids: vec!["../escape".to_string()],
            ..fresh_journal_entry(&source, &target, &before.id)
        };
        move_journal::record(&entry, source.sessions_path())?;
        assert_eq!(journal_entry_count(&source), 1);

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(!outcome.repaired);
        assert_eq!(journal_entry_count(&source), 1, "entry stays on disk");
        let journal_path = first_journal_path(&source);
        assert!(
            unusable_journal_entries_contains(&journal_path),
            "invalid-id entry is blacklisted"
        );
        Ok(())
    }

    #[test]
    fn post_repair_load_error_prevents_home_reload_and_keeps_report() -> Result<()> {
        let (temp, _guard, source, target, before, _after) = setup_recovery_env("reload-fallback")?;
        push_copy(&target, &before)?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        move_journal::record(&entry, source.sessions_path())?;
        let bad_dir = temp.path().join("bad");
        fs::create_dir_all(&bad_dir)?;
        let bad = Storage::new_for_test_path("bad", bad_dir.join("sessions.json"));
        fs::write(bad.sessions_path(), b"not-json")?;
        let outcome = reconcile_loaded(&[&source, &target, &bad]);

        assert!(source.load()?.is_empty(), "repair reached disk");
        assert_eq!(target.load()?.len(), 1);
        assert!(
            !outcome.repaired,
            "Home must keep pre-repair loads instead of repeating the failed reload"
        );
        assert_eq!(outcome.reports.len(), 1, "ambiguity remains surfaced");
        Ok(())
    }

    #[test]
    fn target_still_holds_checks_every_loser_id() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        let winner = Instance::new("winner", "/repo/winner");
        let bystander = Instance::new("bystander", "/repo/bystander");
        let rows = vec![winner.clone(), bystander.clone()];
        fs::write(&path, serde_json::to_vec_pretty(&rows)?)?;

        assert!(target_still_holds(&path, std::slice::from_ref(&winner.id))?);
        assert!(!target_still_holds(
            &path,
            &[winner.id.clone(), "gone".to_string()]
        )?);

        let mut corrupt_row = serde_json::Map::new();
        corrupt_row.insert("id".to_string(), serde_json::Value::from(42));
        let mixed = vec![
            serde_json::Value::Object(corrupt_row),
            serde_json::to_value(&winner)?,
        ];
        fs::write(&path, serde_json::to_vec_pretty(&mixed)?)?;
        assert!(target_still_holds(&path, std::slice::from_ref(&winner.id))?);

        fs::remove_file(&path)?;
        assert!(!target_still_holds(&path, &[winner.id])?);
        Ok(())
    }

    #[test]
    fn same_profile_duplicate_id_is_surfaced() -> Result<()> {
        let (_temp, _guard, source, _target, before, _after) = setup_recovery_env("intraprofile")?;
        source.update(|instances, _| {
            instances.push(before.clone());
            Ok(())
        })?;

        let outcome = reconcile_loaded(&[&source]);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 1, "the repeated id surfaces");
        let report = &outcome.reports[0];
        assert_eq!(report.id, before.id);
        assert!(report.actionable_message().contains(&before.id));
        assert_eq!(source.load()?.len(), 2, "nothing is deleted automatically");
        Ok(())
    }

    fn journal_entry_count(source: &Storage) -> usize {
        move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .len()
    }

    fn journal_entry_scan_ids(source: &Storage) -> Vec<String> {
        move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .into_iter()
            .filter_map(|(_, parsed)| parsed.ok())
            .flat_map(|entry| entry.ids)
            .collect()
    }

    #[test]
    fn group_repair_scope_matches_apply_group_move() -> Result<()> {
        let cases = [
            ("gscope-single", false, true, true),
            ("gscope-subtree", true, false, false),
        ];
        for (tag, move_subtree, path_survives, child_survives) in cases {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(tag)?;
            source.update(|_instances, groups| {
                let mut child = Group::new("archive", "work/archive");
                child.collapsed = true;
                groups.push(child);
                Ok(())
            })?;
            push_copy(&target, &before)?;
            let entry = move_journal::MoveJournalEntry {
                group_move_subtree: move_subtree,
                ..fresh_journal_entry(&source, &target, &before.id)
            };
            move_journal::record(&entry, source.sessions_path())?;

            let outcome = reconcile_loaded(&[&source, &target]);

            assert!(outcome.repaired, "{tag}");
            assert!(outcome.reports.is_empty(), "{tag}");
            assert!(source.load()?.is_empty(), "{tag}: loser emptied");
            let source_groups = source.load_with_groups()?.1;
            assert_eq!(
                source_groups.iter().any(|group| group.path == "work"),
                path_survives,
                "{tag}: moved-path row must mirror apply_group_move"
            );
            assert_eq!(
                source_groups
                    .iter()
                    .any(|group| group.path == "work/archive"),
                child_survives,
                "{tag}: explicit descendant handling must mirror apply_group_move"
            );
            assert_eq!(journal_entry_count(&source), 0, "{tag}");
        }
        Ok(())
    }

    #[test]
    fn chained_move_journals_apply_newest_intent_first() -> Result<()> {
        let (_temp, _guard, a, b, before, _after) = setup_recovery_env("chain")?;
        push_copy(&b, &before)?;
        let now = now_ms();
        let mut j1 = fresh_journal_entry(&a, &b, &before.id);
        j1.created_at_epoch_ms = now;
        let mut j2 = fresh_journal_entry(&b, &a, &before.id);
        j2.group_move_source_path = "moved".to_string();
        j2.group_move_target_path = "work".to_string();
        j2.created_at_epoch_ms = now;
        move_journal::record(&j1, a.sessions_path())?;
        move_journal::record(&j2, b.sessions_path())?;

        let outcome = reconcile_loaded(&[&a, &b]);

        assert!(outcome.repaired);
        assert!(outcome.reports.is_empty());
        assert_eq!(a.load()?.len(), 1, "new reverse-move target survives");
        assert!(b.load()?.is_empty(), "superseded profile is emptied");
        assert_eq!(journal_entry_count(&a), 0);
        assert_eq!(journal_entry_count(&b), 0);
        Ok(())
    }

    #[test]
    fn group_repair_preserves_indirect_metadata_and_other_side_name() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("gparity")?;
        let archived_at = chrono::Utc::now();
        source.update(|_instances, groups| {
            let work = groups
                .iter_mut()
                .find(|group| group.path == "work")
                .unwrap();
            work.collapsed = true;
            work.archived_at = Some(archived_at);
            let mut indirect = Group::new("b", "work/a/b");
            indirect.collapsed = true;
            groups.push(indirect);
            let mut bystander = Group::new("moved", "moved");
            bystander.collapsed = true;
            groups.push(bystander);
            Ok(())
        })?;
        push_copy(&target, &before)?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        move_journal::record(&entry, source.sessions_path())?;

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(outcome.repaired);
        let groups = source.load_with_groups()?.1;
        let work = groups.iter().find(|group| group.path == "work").unwrap();
        assert!(
            work.collapsed,
            "indirect descendant preserves parent metadata"
        );
        assert_eq!(work.archived_at, Some(archived_at));
        assert!(groups.iter().any(|group| group.path == "work/a/b"));
        let bystander = groups.iter().find(|group| group.path == "moved").unwrap();
        assert!(
            bystander.collapsed,
            "other-side name is unrelated on source"
        );
        Ok(())
    }

    #[test]
    fn journal_is_durable_before_external_effect_runs() -> Result<()> {
        let (_temp, _guard, source, target, before, after) = setup_recovery_env("effect-order")?;
        let effect_ran = std::cell::Cell::new(false);
        let crash = ArmedCrashPoint::arm("profile-move-journal");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = source.move_instances_to_inner(
                &target,
                &[(before, after)],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                    account_swap: false,
                },
                |_existing, _candidates| Ok(()),
                |_| {
                    effect_ran.set(true);
                    Ok(())
                },
                sync_resolved_parent_directory,
            );
        }));
        drop(crash);
        assert!(result.is_err(), "journal crash point must fire");
        assert!(
            !effect_ran.get(),
            "journal must precede the external effect"
        );
        assert_eq!(journal_entry_count(&source), 1);
        Ok(())
    }

    #[test]
    fn failed_repair_directory_sync_retains_journal() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("sync-fail")?;
        push_copy(&target, &before)?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        let journal_path = move_journal::record(&entry, source.sessions_path())?;
        let stores = stores(&[&source, &target]);
        let error = repair_journal_entry_with_sync(&entry, &stores, &journal_path, |_path| {
            Err(anyhow!("forced repaired-profile sync failure"))
        })
        .expect_err("failed durability barrier must fail recovery completion");
        assert!(error.to_string().contains("not made durable"));
        assert!(source.load()?.is_empty(), "repair row write reached disk");
        assert_eq!(journal_entry_count(&source), 1, "evidence must remain");

        let retry_error = repair_journal_entry_with_sync(&entry, &stores, &journal_path, |_path| {
            Err(anyhow!("forced retry sync failure"))
        })
        .expect_err("no-loser retry must repeat the durability barrier");
        assert!(retry_error.to_string().contains("not made durable"));
        assert_eq!(journal_entry_count(&source), 1, "retry keeps evidence too");

        let outcome = reconcile_loaded(&[&source, &target]);
        assert!(outcome.repaired, "rerun consumes retained evidence safely");
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn repaired_profile_sync_covers_sessions_and_groups_paths() -> Result<()> {
        let (_temp, _guard, source, _target, _before, _after) = setup_recovery_env("sync-both")?;
        let mut calls = Vec::new();

        sync_repaired_profile_durably(&source, |path| {
            calls.push(path.to_path_buf());
            Ok(())
        })?;

        assert_eq!(
            calls,
            vec![
                source.sessions_path().to_path_buf(),
                source.sessions_path().with_file_name("groups.json"),
            ]
        );
        Ok(())
    }

    #[test]
    fn backup_pruning_syncs_the_lexical_backup_directory() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("profile/sessions.json");
        fs::create_dir_all(path.parent().unwrap())?;
        for stamp in 1..=4 {
            fs::write(
                path.with_file_name(format!("sessions.json.pre-recovery-{stamp}")),
                stamp.to_string(),
            )?;
        }
        let mut synced = Vec::new();

        prune_old_recovery_backups_with_sync(&path, 3, |candidate| {
            synced.push(candidate.to_path_buf());
            Ok(())
        })?;

        assert_eq!(synced, vec![path]);
        Ok(())
    }

    #[test]
    fn recovery_backup_retention_keeps_newest_three() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        fs::write(&path, b"[]")?;
        for stamp in 1..=5 {
            fs::write(
                temp.path()
                    .join(format!("sessions.json.pre-recovery-{stamp}")),
                stamp.to_string(),
            )?;
        }
        backup_before_repair(&path)?;
        let mut stamps: Vec<u128> = fs::read_dir(temp.path())?
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter_map(|name| {
                name.to_string_lossy()
                    .strip_prefix("sessions.json.pre-recovery-")
                    .and_then(|value| value.parse().ok())
            })
            .collect();
        stamps.sort();
        assert_eq!(stamps.len(), RECOVERY_BACKUPS_TO_KEEP);
        assert_eq!(&stamps[..2], &[4, 5], "oldest backups are pruned");
        Ok(())
    }

    #[test]
    fn post_repair_load_error_preserves_pre_repair_report() -> Result<()> {
        let temp = tempdir()?;
        let good_dir = temp.path().join("good");
        let bad_dir = temp.path().join("bad");
        fs::create_dir_all(&good_dir)?;
        fs::create_dir_all(&bad_dir)?;
        let good = Storage::new_for_test_path("good", good_dir.join("sessions.json"));
        let bad = Storage::new_for_test_path("bad", bad_dir.join("sessions.json"));
        let row = Instance::new("duplicate", "/repo/duplicate");
        good.update(|instances, _| {
            instances.push(row.clone());
            Ok(())
        })?;
        fs::write(bad.sessions_path(), b"not-json")?;
        let good_rows = good.load()?;
        let fallback_bad = vec![row.clone()];
        let fallback: Vec<(&str, &[Instance])> = vec![
            ("good", good_rows.as_slice()),
            ("bad", fallback_bad.as_slice()),
        ];
        let stores: Vec<(&str, &Storage)> = vec![("good", &good), ("bad", &bad)];

        let (reports, reload_succeeded) = reports_after_repair(&fallback, &stores);

        assert!(!reload_succeeded, "Home must keep its pre-repair loads");
        assert_eq!(reports.len(), 1, "load error keeps ambiguity surfaced");
        assert_eq!(reports[0].id, row.id);
        Ok(())
    }

    fn fresh_journal_entry(
        source: &Storage,
        target: &Storage,
        id: &str,
    ) -> move_journal::MoveJournalEntry {
        move_journal::MoveJournalEntry {
            version: move_journal::MOVE_JOURNAL_VERSION,
            ids: vec![id.to_string()],
            source_profile: source.profile().to_string(),
            target_profile: target.profile().to_string(),
            source_sessions_path: source.sessions_path().to_path_buf(),
            target_sessions_path: target.sessions_path().to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: now_ms(),
        }
    }

    #[test]
    fn recovery_survives_a_panic_mid_repair_and_stays_idempotent() -> Result<()> {
        let (_temp, _guard, source, target, _before, _after) = setup_recovery_env("midpanic")?;
        run_crashing_move(&source, &target, "profile-move-source-sessions");
        assert_eq!(journal_entry_count(&source), 1);

        {
            let _crash = ArmedCrashPoint::arm("profile-repair-source-written");
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reconcile_loaded(&[&source, &target]);
            }));
        }
        let outcome = reconcile_loaded(&[&source, &target]);
        assert!(outcome.repaired || journal_entry_count(&source) == 0);
        assert!(outcome.reports.is_empty());
        assert!(source.load()?.is_empty());
        assert_eq!(target.load()?.len(), 1);
        assert_eq!(journal_entry_count(&source), 0);

        let outcome = reconcile_loaded(&[&source, &target]);
        assert!(
            !outcome.repaired && outcome.reports.is_empty(),
            "rerun is a no-op"
        );
        Ok(())
    }

    fn reconcile_loaded(storages: &[&Storage]) -> ReconciliationOutcome {
        let loaded: Vec<_> = storages
            .iter()
            .map(|s| s.load().unwrap_or_default())
            .collect();
        let refs: Vec<(&str, &[Instance])> = storages
            .iter()
            .zip(&loaded)
            .map(|(s, rows)| (s.profile(), rows.as_slice()))
            .collect();
        reconcile_profile_duplicates(&refs, &stores(storages))
    }

    fn stores<'a>(storages: &[&'a Storage]) -> Vec<(&'a str, &'a Storage)> {
        storages.iter().map(|s| (s.profile(), *s)).collect()
    }

    fn push_copy(storage: &Storage, instance: &Instance) -> Result<()> {
        storage.update(|instances, _| {
            let mut copy = instance.clone();
            copy.source_profile = storage.profile().to_string();
            instances.push(copy);
            Ok(())
        })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn first_journal_path(storage: &Storage) -> PathBuf {
        move_journal::scan([storage.sessions_path().to_path_buf()])
            .entries
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .expect("journal entry on disk")
    }
}

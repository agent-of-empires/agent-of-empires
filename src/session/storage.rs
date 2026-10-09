//! Session storage - JSON file persistence with in-process and cross-process locking.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;

use super::{
    get_app_dir, get_profile_dir, get_profile_dir_locked, get_profile_dir_path,
    resolve_existing_profile, resolve_profile_name_locked, Group, Instance,
};

/// Sidecar lock file name for per-profile storage.
pub(crate) const STORAGE_LOCK_FILENAME: &str = ".storage.lock";

/// Sidecar lock file name for the global workspace-ordering file.
const WORKSPACE_LOCK_FILENAME: &str = ".workspace-ordering.lock";
/// Sidecar lock prefix for one session's launch lifecycle.
const INSTANCE_LIFECYCLE_LOCK_PREFIX: &str = ".instance-lifecycle-";
/// Sidecar lock for every mutation that can create or change a session's `(title,
/// project_path)` identity.
pub(crate) const SESSION_IDENTITY_LOCK_FILENAME: &str = ".title-mutation.lock";
/// Sidecar lock for claims on managed workspace paths.
pub(crate) const SESSION_WORKSPACE_CLAIM_LOCK_FILENAME: &str = ".workspace-claim.lock";
/// Sidecar lock for profile namespace rename/delete and storage writes.
pub(crate) const PROFILE_NAMESPACE_LOCK_FILENAME: &str = ".profile-namespace.lock";
/// Physical inode stamp. Missing native creation time is never durable birth authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    #[serde(default)]
    pub(crate) birth_time: Option<std::time::SystemTime>,
}

impl DirectoryIdentity {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (device, inode) = (0, 0);
        Self {
            device,
            inode,
            birth_time: metadata.created().ok(),
        }
    }

    pub(crate) fn is_durable(&self) -> bool {
        cfg!(unix) && self.birth_time.is_some()
    }
}

fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading profile directory identity {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "profile identity path is not a directory"
    );
    Ok(DirectoryIdentity::from_metadata(&metadata))
}

fn capture_directory(path: &Path) -> Result<(DirectoryIdentity, Arc<File>)> {
    let file = File::open(path)
        .with_context(|| format!("pinning original profile directory {}", path.display()))?;
    let opened = file.metadata()?;
    let current = fs::metadata(path)?;
    anyhow::ensure!(
        opened.is_dir() && super::same_filesystem_identity(&opened, &current),
        "original profile directory changed while acquiring its inode pin"
    );
    let identity = DirectoryIdentity::from_metadata(&opened);
    Ok((identity, Arc::new(file)))
}

pub(crate) fn acquire_profile_namespace_lock() -> Result<StorageFlock> {
    acquire_storage_flock(&get_app_dir()?, PROFILE_NAMESPACE_LOCK_FILENAME)
}
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
    replace_file_no_follow_as(root, rel, content, 0o644)
}

/// [`replace_file_no_follow`] with an explicit mode, for content that must not
/// stay world readable.
#[cfg(unix)]
pub(crate) fn replace_file_no_follow_as(
    root: &Path,
    rel: &Path,
    content: &[u8],
    mode: u32,
) -> Result<()> {
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
        Mode::from_bits_truncate(mode as libc::mode_t),
    )
    .with_context(|| format!("creating a temp file under {}", root.display()))?;

    let written = (|| -> Result<()> {
        let mut file = fs::File::from(tmp);
        file.write_all(content)?;
        fchmod(&file, Mode::from_bits_truncate(mode as libc::mode_t))?;
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
pub(crate) fn save_lock_for(profile: &str) -> Arc<Mutex<()>> {
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
    own: HeldFlock,
    sibling: Option<HeldFlock>,
}

struct HeldFlock {
    file: fs::File,
}

impl StorageFlock {
    pub(crate) fn file_identity(&self) -> Result<DirectoryIdentity> {
        Ok(DirectoryIdentity::from_metadata(&self.own.file.metadata()?))
    }
}

/// A held file supplies an immutable flock rank, not native execution authority.
pub(crate) struct OpenStorageLock {
    file: fs::File,
    path: PathBuf,
    key: (u64, u64),
}

impl OpenStorageLock {
    pub(crate) fn new(file: fs::File, path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(Self {
            key: (metadata.dev(), metadata.ino()),
            file,
            path,
        })
    }

    pub(crate) fn physical_key(&self) -> (u64, u64) {
        self.key
    }

    pub(crate) fn acquire(self, shared: bool) -> Result<StorageFlock> {
        if shared {
            acquire_open_storage_shared_flock(self.file, &self.path)
        } else {
            acquire_open_storage_flock(self.file, &self.path)
        }
    }
}

fn acquire_open_storage_flocks(
    mut files: Vec<OpenStorageLock>,
    shared: bool,
) -> Result<Vec<StorageFlock>> {
    files.sort_unstable_by_key(OpenStorageLock::physical_key);
    files.dedup_by_key(|file| file.physical_key());
    files.into_iter().map(|file| file.acquire(shared)).collect()
}

pub(crate) fn acquire_storage_flock_cohort<'a>(
    directories: impl IntoIterator<Item = &'a Path>,
    name: &str,
    shared: bool,
) -> Result<Vec<StorageFlock>> {
    let files = directories
        .into_iter()
        .map(|directory| {
            let (file, path) = open_storage_lock_file(directory, name)?;
            OpenStorageLock::new(file, path)
        })
        .collect::<Result<Vec<_>>>()?;
    acquire_open_storage_flocks(files, shared)
}

fn lock_two_save_mutexes<'a>(
    source: &'a Storage,
    target: &'a Storage,
) -> (
    std::sync::MutexGuard<'a, ()>,
    Option<std::sync::MutexGuard<'a, ()>>,
) {
    let (first, second) = if Arc::as_ptr(&source.save_lock) < Arc::as_ptr(&target.save_lock) {
        (&source.save_lock, &target.save_lock)
    } else {
        (&target.save_lock, &source.save_lock)
    };
    let first_guard = first
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let second_guard = (!Arc::ptr_eq(first, second)).then(|| {
        second
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    });
    (first_guard, second_guard)
}

impl std::os::fd::AsFd for StorageFlock {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.own.file.as_fd()
    }
}

impl Drop for HeldFlock {
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
    acquire_storage_flock_cohort(
        app_dirs.iter().map(PathBuf::as_path),
        crate::migrations::v027_isolate_sandbox_stores::LOCK,
        true,
    )
}

#[cfg(unix)]
pub(crate) fn same_filesystem_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
pub(crate) fn same_filesystem_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
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

/// Like [`open_storage_lock_file`] but for a directory that must already
/// exist: a superseded writer must not bring a deleted or renamed profile back
/// as an empty directory. The open itself also fails on a directory that
/// disappeared in between, and `verify_profile_identity` still rejects one that
/// came back under a new identity.
fn open_existing_storage_lock_file(dir: &Path, name: &str) -> Result<(fs::File, PathBuf)> {
    if !dir.is_dir() {
        anyhow::bail!("profile directory is gone: {}", dir.display());
    }
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

/// [`acquire_storage_flock`] for a directory that must already exist.
fn acquire_existing_storage_flock(dir: &Path, name: &str) -> Result<StorageFlock> {
    let (file, path) = open_existing_storage_lock_file(dir, name)?;
    acquire_open_storage_flock(file, &path)
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

pub(crate) fn acquire_open_storage_flock(file: fs::File, path: &Path) -> Result<StorageFlock> {
    if let Err(e) = file.try_lock_exclusive() {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(e.into());
        }
        #[cfg(debug_assertions)]
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
    Ok(StorageFlock {
        own: HeldFlock { file },
        sibling: None,
    })
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
    Ok(StorageFlock {
        own: HeldFlock { file },
        sibling: None,
    })
}

/// Acquire the app-wide session identity-mutation lock.
pub(crate) fn acquire_session_identity_lock() -> Result<StorageFlock> {
    acquire_storage_flock(&get_app_dir()?, SESSION_IDENTITY_LOCK_FILENAME)
}

/// Serialize path ownership claims without holding the global identity lock over Git work.
pub(crate) fn acquire_session_workspace_claim_lock() -> Result<StorageFlock> {
    let current = get_app_dir()?;
    let Some(sibling) = super::sibling_namespace_app_dir() else {
        return acquire_session_workspace_claim_lock_in(&current);
    };
    fs::create_dir_all(&current)?;
    fs::create_dir_all(&sibling)?;
    let current = current.canonicalize()?;
    let sibling = sibling.canonicalize()?;
    if paths_share_filesystem_identity(&current, &sibling)? {
        return acquire_session_workspace_claim_lock_in(&current);
    }
    let (current_file, current_path) =
        open_storage_lock_file(&current, SESSION_WORKSPACE_CLAIM_LOCK_FILENAME)?;
    let (sibling_file, sibling_path) =
        open_storage_lock_file(&sibling, SESSION_WORKSPACE_CLAIM_LOCK_FILENAME)?;
    let current_file = OpenStorageLock::new(current_file, current_path)?;
    let sibling_file = OpenStorageLock::new(sibling_file, sibling_path)?;
    if current_file.physical_key() == sibling_file.physical_key() {
        return current_file.acquire(false);
    }
    let (mut own, other) = if current_file.physical_key() < sibling_file.physical_key() {
        let own = current_file.acquire(false)?;
        (own, sibling_file.acquire(false)?)
    } else {
        let other = sibling_file.acquire(false)?;
        (current_file.acquire(false)?, other)
    };
    own.sibling = Some(other.own);
    Ok(own)
}

pub(crate) fn acquire_session_workspace_claim_lock_in(app_dir: &Path) -> Result<StorageFlock> {
    acquire_storage_flock(app_dir, SESSION_WORKSPACE_CLAIM_LOCK_FILENAME)
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
        Ok(()) => Ok(Some(StorageFlock {
            own: HeldFlock { file },
            sibling: None,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Clone)]
pub struct Storage {
    profile: String,
    sessions_path: PathBuf,
    save_lock: Arc<Mutex<()>>,
    file_watch: Arc<FileWatchService>,
    profile_identity: Option<DirectoryIdentity>,
    // Pins the actual captured directory inode across every original Storage clone.
    profile_directory: Option<Arc<File>>,
    #[cfg(test)]
    fail_writes_for_test: bool,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("profile", &self.profile)
            .field("sessions_path", &self.sessions_path)
            .field("identity", &self.profile_identity)
            .finish_non_exhaustive()
    }
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

#[derive(Clone)]
pub(crate) enum MetadataSelection<'a> {
    Session(std::borrow::Cow<'a, str>),
    Sessions(std::borrow::Cow<'a, [String]>),
    Instances(&'a [Instance]),
    Identifier(std::borrow::Cow<'a, str>),
    Group(std::borrow::Cow<'a, str>),
    Groups(std::borrow::Cow<'a, [String]>),
    Subtree(std::borrow::Cow<'a, str>),
    GroupAssignment {
        identifier: std::borrow::Cow<'a, str>,
        group: std::borrow::Cow<'a, str>,
    },
    Ordering {
        anchor: std::borrow::Cow<'a, str>,
        source: std::borrow::Cow<'a, str>,
        destination: std::borrow::Cow<'a, str>,
    },
    GroupOrdering {
        anchor: std::borrow::Cow<'a, str>,
        overlays: &'a HashMap<String, bool>,
    },
    AllSessions,
}

struct MetadataAdmission<'a> {
    sessions: std::collections::HashSet<&'a str>,
    groups: std::collections::HashSet<&'a str>,
}

fn in_subtree(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn admit_metadata_owner<'a>(
    document: &'a super::raw_document::RowDocument,
    id: &str,
    selected: &mut std::collections::HashSet<&'a str>,
) -> Result<()> {
    document.admit(id)?;
    if let Some((id, _)) = document.owners.get_key_value(id) {
        selected.insert(id);
    }
    Ok(())
}

fn resolve_metadata_identifier<'a>(
    document: &'a super::raw_document::RowDocument,
    identifier: &str,
) -> Result<&'a str> {
    if let Some((id, _)) = document.owners.get_key_value(identifier) {
        return Ok(id);
    }
    let mut prefixes = document
        .owners
        .keys()
        .filter(|id| id.starts_with(identifier));
    if let Some(id) = prefixes.next() {
        anyhow::ensure!(
            prefixes.next().is_none(),
            "Ambiguous session identifier {identifier}"
        );
        return Ok(id);
    }
    for field in ["title", "project_path"] {
        for raw in &document.raw.rows {
            let Ok(object) = super::raw_document::RawObject::parse(raw) else {
                continue;
            };
            if object.strings(field).any(|value| value == identifier) {
                let id: String = serde_json::from_str(
                    object
                        .unique("id")?
                        .context("matching row has no unique owner")?
                        .get(),
                )?;
                return document
                    .owners
                    .get_key_value(&id)
                    .map(|(id, _)| id.as_str())
                    .context("matching owner is absent");
            }
        }
    }
    anyhow::bail!("Session not found: {identifier}")
}

impl MetadataSelection<'_> {
    fn admit<'a>(
        &self,
        sessions: &'a super::raw_document::RowDocument,
        groups: &'a super::raw_document::RowDocument,
    ) -> Result<MetadataAdmission<'a>> {
        let mut admitted = MetadataAdmission {
            sessions: Default::default(),
            groups: Default::default(),
        };
        let mut select_session =
            |id: &str| admit_metadata_owner(sessions, id, &mut admitted.sessions);
        match self {
            Self::Session(id) => select_session(id)?,
            Self::Sessions(ids) => {
                for id in ids.iter() {
                    select_session(id)?;
                }
            }
            Self::Instances(rows) => {
                for row in *rows {
                    select_session(&row.id)?;
                }
            }
            Self::Identifier(identifier) | Self::GroupAssignment { identifier, .. } => {
                select_session(resolve_metadata_identifier(sessions, identifier)?)?
            }
            Self::AllSessions => {
                sessions.admit_all()?;
                for id in sessions.owners.keys() {
                    admitted.sessions.insert(id);
                }
            }
            Self::Ordering { anchor, .. } => select_session(anchor)?,
            Self::Group(_) | Self::Groups(_) | Self::Subtree(_) | Self::GroupOrdering { .. } => {}
        }
        let member_matches = |path: &str| match self {
            Self::Subtree(root) => in_subtree(path, root),
            Self::Ordering {
                source,
                destination,
                ..
            } => path == source.as_ref() || path == destination.as_ref(),
            _ => false,
        };
        if matches!(self, Self::Subtree(_) | Self::Ordering { .. }) {
            for raw in &sessions.raw.rows {
                let object = super::raw_document::RawObject::parse(raw)
                    .context("unreadable group membership")?;
                let field = object.unique("group_path")?;
                let path: String = field
                    .map(|value| serde_json::from_str(value.get()))
                    .transpose()?
                    .unwrap_or_default();
                if member_matches(&path) {
                    let id: String = serde_json::from_str(
                        object
                            .unique("id")?
                            .context("group member has no owner identity")?
                            .get(),
                    )?;
                    admit_metadata_owner(sessions, &id, &mut admitted.sessions)?;
                }
            }
        }
        let mut select_group =
            |path: &str| admit_metadata_owner(groups, path, &mut admitted.groups);
        match self {
            Self::Group(path) | Self::GroupAssignment { group: path, .. } => {
                for ancestor in path
                    .match_indices('/')
                    .map(|(end, _)| &path[..end])
                    .chain(std::iter::once(path.as_ref()))
                {
                    select_group(ancestor)?;
                }
            }
            Self::Groups(paths) => {
                for path in paths.iter() {
                    select_group(path)?;
                }
            }
            Self::Subtree(root) => {
                for path in groups.owners.keys().filter(|path| in_subtree(path, root)) {
                    select_group(path)?;
                }
            }
            Self::Ordering {
                source,
                destination,
                ..
            } => {
                select_group(source)?;
                select_group(destination)?;
            }
            Self::GroupOrdering { anchor, overlays } => {
                select_group(anchor)?;
                for path in overlays.keys() {
                    select_group(path)?;
                }
                let parent = anchor.rsplit_once('/').map(|(parent, _)| parent);
                for path in groups
                    .owners
                    .keys()
                    .filter(|path| path.rsplit_once('/').map(|(parent, _)| parent) == parent)
                {
                    select_group(path)?;
                }
            }
            _ => {}
        }
        Ok(admitted)
    }
}

enum StorageWriteScope<'a> {
    Metadata(MetadataSelection<'a>),
    Geometry,
    GeometryOwner(&'a str),
    FencedGeometry(&'a super::deletion::PathClaimIndex),
    CompletePaths {
        original: &'a super::LaunchOrigin,
        paths: &'a [PathBuf],
    },
    CompleteCreation {
        original: &'a super::builder::CreationIntent,
    },
}

struct StoredWriteGeometry {
    created_at: chrono::DateTime<chrono::Utc>,
    paths: Vec<PathBuf>,
    pending: Option<super::LifecycleReservation>,
}

impl StoredWriteGeometry {
    fn capture(row: &Instance) -> Self {
        Self {
            created_at: row.created_at,
            paths: row.durable_worktree_paths().map(PathBuf::from).collect(),
            pending: row
                .lifecycle_reservation
                .as_ref()
                .filter(|lease| lease.path_claims.is_pending())
                .cloned(),
        }
    }

    fn matches(&self, row: &Instance) -> bool {
        self.created_at == row.created_at
            && self
                .paths
                .iter()
                .map(PathBuf::as_path)
                .eq(row.durable_worktree_paths())
    }
}

impl Storage {
    pub(crate) fn original_profile_identity(&self) -> Result<DirectoryIdentity> {
        anyhow::ensure!(
            self.profile_directory.is_some(),
            "original profile inode pin is unavailable"
        );
        self.profile_identity
            .context("original physical profile identity is unavailable")
    }
    pub(crate) fn original_profile_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.profile_directory
            .as_ref()
            .map(|directory| directory.as_fd())
            .context("original profile inode pin is unavailable")
    }

    pub(crate) fn load_claim_abort_document_locked(
        &self,
    ) -> Result<super::raw_document::RawDocument> {
        self.verify_profile_identity()?;
        let anchor = super::AnchoredDir::duplicate_original(
            self.sessions_path
                .parent()
                .context("sessions path has no parent")?,
            self.original_profile_fd()?,
        )?;
        let bytes = anchor
            .read_regular(Path::new("sessions.json"), usize::MAX)?
            .unwrap_or_default();
        super::raw_document::RawDocument::parse(std::str::from_utf8(&bytes)?)
    }

    pub(crate) fn publish_claim_abort_document_locked(&self, mut bytes: &[u8]) -> Result<()> {
        self.verify_profile_identity()?;
        #[cfg(test)]
        anyhow::ensure!(!self.fail_writes_for_test, "injected storage write failure");
        let anchor = super::AnchoredDir::duplicate_original(
            self.sessions_path
                .parent()
                .context("sessions path has no parent")?,
            self.original_profile_fd()?,
        )?;
        anchor.publish_file(
            Path::new("sessions.json"),
            &mut bytes,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
            true,
            None,
        )?;
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(())
    }

    pub(crate) fn same_origin_as(&self, other: &Self) -> bool {
        self.profile_identity.is_some() && self.profile_identity == other.profile_identity
    }
    pub fn new(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = if profile.is_empty() {
            super::config::resolve_default_profile()
        } else {
            profile.to_string()
        };

        let profile_dir = get_profile_dir(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);
        let (profile_identity, profile_directory) = capture_directory(&profile_dir)?;

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
            profile_identity: Some(profile_identity),
            profile_directory: Some(profile_directory),
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }

    /// Construct a `Storage` wired to a noop `FileWatchService`.
    pub fn new_unwatched(profile: &str) -> Result<Self> {
        Self::new(profile, FileWatchService::noop())
    }

    #[cfg(test)]
    /// Construct a `Storage` at an explicit `sessions.json` path, for tests
    /// that need a path the profile registry would not produce. It creates the
    /// profile directory, which the lock openers no longer do implicitly.
    pub(crate) fn new_for_test_path(profile: &str, sessions_path: PathBuf) -> Self {
        if let Some(dir) = sessions_path.parent() {
            std::fs::create_dir_all(dir).expect("test profile directory");
        }
        let profile_dir = sessions_path.parent().expect("test profile directory");
        let (profile_identity, profile_directory) =
            capture_directory(profile_dir).expect("test original directory pin");
        Self {
            profile: profile.to_string(),
            sessions_path,
            save_lock: save_lock_for(profile),
            file_watch: FileWatchService::noop(),
            profile_identity: Some(profile_identity),
            profile_directory: Some(profile_directory),
            fail_writes_for_test: false,
        }
    }

    pub(crate) fn adopt_original_profile(
        profile: String,
        directory: File,
        expected: DirectoryIdentity,
    ) -> Result<Self> {
        let metadata = directory.metadata()?;
        anyhow::ensure!(
            !profile.is_empty()
                && metadata.is_dir()
                && expected.is_durable()
                && DirectoryIdentity::from_metadata(&metadata) == expected,
            "received FD is not the issued original profile directory"
        );
        let sessions_path = get_profile_dir_path(&profile)?.join("sessions.json");
        let storage = Self {
            save_lock: save_lock_for(&profile),
            profile,
            sessions_path,
            file_watch: FileWatchService::noop(),
            profile_identity: Some(expected),
            profile_directory: Some(Arc::new(directory)),
            #[cfg(test)]
            fail_writes_for_test: false,
        };
        storage.verify_profile_identity()?;
        Ok(storage)
    }

    /// Construct a `Storage` for an existing profile, never creating it.
    pub fn open(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = resolve_existing_profile(profile)?;
        let profile_dir = get_profile_dir_path(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);
        let (profile_identity, profile_directory) = capture_directory(&profile_dir)?;

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
            profile_identity: Some(profile_identity),
            profile_directory: Some(profile_directory),
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }
    /// [`Storage::open`] for a path that creates the session it will store, where
    /// the profile may not exist yet.
    ///
    /// The caller must already hold the session identity lock, the one every
    /// profile delete and rename also takes, so the creation cannot land outside
    /// that window. A teardown or a read must use [`Storage::open`], which
    /// refuses a profile that is not there rather than bringing one back.
    /// Creating constructor: the caller holds the session identity flock, so the
    /// name is resolved with the locked variant. The unlocked one can reach the
    /// first-profile bootstrap, which takes that flock again on a fresh
    /// descriptor and would wait on the caller's own lock.
    pub(crate) fn open_or_create(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = resolve_profile_name_locked(profile)?;
        let profile_dir = get_profile_dir_locked(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);
        let (profile_identity, profile_directory) = capture_directory(&profile_dir)?;

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
            profile_identity: Some(profile_identity),
            profile_directory: Some(profile_directory),
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }

    /// [`Storage::open`] wired to a noop `FileWatchService`.
    pub fn open_unwatched(profile: &str) -> Result<Self> {
        Self::open(profile, FileWatchService::noop())
    }

    /// Reopen only the original physical profile, retaining the watch service.
    /// Adopting a replacement profile requires an explicit fresh open by its owner.
    pub(crate) fn reopen_preserving_watch(&self) -> Result<Storage> {
        self.verify_profile_identity()?;
        let reopened = Self::open(&self.profile, self.file_watch.clone())?;
        self.verify_profile_identity()?;
        Ok(reopened)
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
        acquire_existing_storage_flock(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )
    }

    /// Whether some holder currently has `instance_id`'s lifecycle lock.
    #[cfg(test)]
    pub(crate) fn instance_lifecycle_lock_is_held_for_test(&self, instance_id: &str) -> bool {
        let profile_dir = self.sessions_path.parent().expect("profile directory");
        try_acquire_storage_flock(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )
        .expect("probe lifecycle lock")
        .is_none()
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
        self.verify_profile_identity()?;
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
        let origin = Arc::new(self.clone());
        for (idx, row) in rows.into_iter().enumerate() {
            match <Instance as serde::Deserialize>::deserialize(&row) {
                Ok(mut inst) => {
                    // `source_profile` is never persisted, so a loaded row
                    // would otherwise resolve its agent against the default
                    // profile rather than the store it came from.
                    inst.source_profile = self.profile.clone();
                    inst.storage_origin = Some(origin.clone());
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

        self.verify_profile_identity()?;
        if !corrupt.is_empty() {
            self.quarantine_corrupt_rows(&corrupt);
        }

        Ok(instances)
    }

    fn load_raw_document_locked(&self, path: &Path) -> Result<super::raw_document::RawDocument> {
        self.verify_profile_identity()?;
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.verify_profile_identity()?;
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(error) => {
                        return Err(error).with_context(|| format!("inspecting {}", path.display()))
                    }
                    Ok(_) => anyhow::bail!(
                        "canonical inventory {} exists but cannot be read safely",
                        path.display()
                    ),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        self.verify_profile_identity()?;
        super::raw_document::RawDocument::parse(&content)
            .with_context(|| format!("parsing {}", path.display()))
    }

    /// Loss changes only the claim state; original authority is never reconstructed.
    pub(crate) fn reconcile_filesystem_claims(&self) -> Result<bool> {
        self.reconcile_filesystem_claims_with(crate::process::OriginalCustodianBirth::observe)
    }

    fn reconcile_filesystem_claims_with(
        &self,
        observe: impl Fn(&crate::process::OriginalCustodianBirth) -> crate::process::CustodianLiveness,
    ) -> Result<bool> {
        let _workspace = acquire_session_workspace_claim_lock()?;
        let _identity = acquire_session_identity_lock()?;
        self.verify_profile_identity()?;
        let profile = self.original_profile_identity()?;
        if !profile.is_durable() {
            return Ok(false);
        }
        let mut ids = with_storages_locked(std::slice::from_ref(self), || {
            self.load_raw_document_locked(&self.sessions_path)
                .map(|document| {
                    document
                        .owners("id")
                        .into_iter()
                        .filter_map(|(id, owner)| {
                            (owner.count == 1
                                && !owner.ambiguous
                                && super::claim_reconcile::pending_custodian(
                                    &document.rows[owner.index],
                                    profile,
                                )
                                .ok()
                                .flatten()
                                .is_some())
                            .then_some(id)
                        })
                        .collect::<Vec<_>>()
                })
        })??;
        ids.sort();
        let mut changed = false;
        for id in ids {
            if super::validate_instance_id(&id).is_err() {
                continue;
            }
            let _lifecycle = self.acquire_instance_lifecycle_lock(&id)?;
            changed |= with_storages_locked(std::slice::from_ref(self), || -> Result<bool> {
                self.verify_profile_identity()?;
                let mut document = self.load_raw_document_locked(&self.sessions_path)?;
                if !super::claim_reconcile::mark_lost(&mut document, &id, profile, &observe)? {
                    return Ok(false);
                }
                #[cfg(test)]
                anyhow::ensure!(!self.fail_writes_for_test, "injected storage write failure");
                self.verify_profile_identity()?;
                atomic_write(
                    &self.sessions_path,
                    &serde_json::to_vec_pretty(&document.rows)?,
                )?;
                sync_parent_directory(&self.sessions_path)?;
                self.file_watch.notify_local_change(&self.sessions_path);
                Ok(true)
            })??;
        }
        Ok(changed)
    }

    pub(crate) fn load_path_owners_locked(&self) -> Result<super::deletion::WorktreeOwnerDocument> {
        super::deletion::WorktreeOwnerDocument::project(
            self.load_raw_document_locked(&self.sessions_path)?,
        )
    }

    fn load_projected_sessions_locked(
        &self,
    ) -> Result<(super::raw_document::RowDocument, Vec<Instance>)> {
        let raw = self.load_raw_document_locked(&self.sessions_path)?;
        let (document, mut instances) =
            super::raw_document::RowDocument::project::<Instance>(raw, "id")?;
        let origin = Arc::new(self.clone());
        for instance in &mut instances {
            instance.source_profile = self.profile.clone();
            instance.storage_origin = Some(origin.clone());
            instance.set_file_watch(self.file_watch.clone());
        }
        Ok((document, instances))
    }

    /// Native owner inventories require every row to be uniquely decodable.
    pub(crate) fn load_strict_for_worktree_ownership_locked(&self) -> Result<Vec<Instance>> {
        let (document, instances) = self.load_projected_sessions_locked()?;
        document.admit_all()?;
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
        self.verify_profile_identity()?;
        let _workspace = acquire_session_workspace_claim_lock()?;
        self.update_under_storage_locks(f, StorageWriteScope::Geometry)
    }

    /// Metadata writes cannot change row ownership, paths or filesystem intent.
    pub(crate) fn update_metadata<F, R>(&self, selection: MetadataSelection<'_>, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::Metadata(selection))
    }

    pub(crate) fn complete_path_claims_under_workspace_lock<F, R>(
        &self,
        original: &super::LaunchOrigin,
        paths: &[PathBuf],
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::CompletePaths { original, paths })
    }

    pub(crate) fn complete_creation_under_workspace_claim_lock<F, R>(
        &self,
        original: &super::builder::CreationIntent,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::CompleteCreation { original })
    }

    /// Inspect both endpoints before effects while the caller holds the workspace fence.
    pub(crate) fn ensure_worktree_edit_unclaimed_under_workspace_lock(
        &self,
        id: &str,
        current: &Path,
        new_name: &str,
    ) -> Result<()> {
        let target = super::worktree_edit::target_worktree_path(current, new_name)
            .context("worktree destination has no parent")?;
        self.ensure_worktree_write_unclaimed_under_workspace_lock(
            id,
            current,
            &[current, target.as_path()],
        )
    }

    pub(crate) fn ensure_worktree_branch_unclaimed_under_workspace_lock(
        &self,
        id: &str,
        current: &Path,
    ) -> Result<()> {
        self.ensure_worktree_write_unclaimed_under_workspace_lock(id, current, &[current])
    }

    fn ensure_worktree_write_unclaimed_under_workspace_lock(
        &self,
        id: &str,
        current: &Path,
        candidates: &[&Path],
    ) -> Result<()> {
        self.verify_profile_identity()?;
        let claims = super::deletion::PathClaimIndex::load_for_writer(std::slice::from_ref(self))?;
        let rows = self.load_strict_for_worktree_ownership_locked()?;
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .context("worktree owner disappeared")?;
        anyhow::ensure!(
            Path::new(&row.project_path) == current,
            "worktree source plan was superseded"
        );
        anyhow::ensure!(
            !row.has_pending_worktree_path_claims(),
            "worktree owner has an unfinished filesystem intent"
        );
        claims.ensure_pending_writes_unclaimed(candidates)
    }

    /// Update while the caller already owns the workspace claim lock.
    pub(crate) fn update_under_workspace_claim_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::Geometry)
    }

    /// Admit the selected raw owner before invoking a geometry mutation.
    pub(crate) fn update_owner_under_workspace_claim_lock<F, R>(&self, id: &str, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::GeometryOwner(id))
    }

    /// Reuse the inventory while the caller retains its workspace fence.
    pub(crate) fn update_with_claim_index_under_workspace_lock<F, R>(
        &self,
        claims: &super::deletion::PathClaimIndex,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.verify_profile_identity()?;
        self.update_under_storage_locks(f, StorageWriteScope::FencedGeometry(claims))
    }

    /// Take only this store's locks, after any required workspace fence.
    fn update_under_storage_locks<F, R>(&self, f: F, scope: StorageWriteScope<'_>) -> Result<R>
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
        let _flock = acquire_existing_storage_flock(profile_dir, STORAGE_LOCK_FILENAME)?;
        self.verify_profile_identity()?;
        self.update_under_lock(f, scope)
    }

    pub(crate) fn verify_profile_identity(&self) -> Result<()> {
        let Some(expected) = self.profile_identity else {
            return Ok(());
        };
        let profile_dir = self.sessions_path.parent().ok_or_else(|| {
            anyhow!(
                "sessions_path missing parent: {}",
                self.sessions_path.display()
            )
        })?;
        let actual = directory_identity(profile_dir).with_context(|| {
            format!(
                "profile namespace is no longer available: {}",
                profile_dir.display()
            )
        })?;
        anyhow::ensure!(
            actual == expected,
            "profile namespace changed while this Storage writer was open"
        );
        Ok(())
    }

    /// Apply one storage mutation while the caller already owns this profile's
    /// in-process save lock and cross-process storage flock.
    fn update_under_lock<F, R>(&self, f: F, scope: StorageWriteScope<'_>) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        let (session_document, mut instances) = self.load_projected_sessions_locked()?;
        let raw_groups =
            self.load_raw_document_locked(&self.sessions_path.with_file_name("groups.json"))?;
        let (group_document, mut groups) =
            super::raw_document::RowDocument::project::<Group>(raw_groups, "path")?;
        if let StorageWriteScope::GeometryOwner(id) = &scope {
            session_document.admit(id)?;
            if !session_document.owners.contains_key(*id) {
                session_document.admit_all()?;
            }
        }
        let selected = match &scope {
            StorageWriteScope::Metadata(selection) => {
                Some(selection.admit(&session_document, &group_document)?)
            }
            _ => None,
        };
        let before: HashMap<_, _> = instances
            .iter()
            .map(|row| (row.id.clone(), StoredWriteGeometry::capture(row)))
            .collect();
        let completing = match &scope {
            StorageWriteScope::CompletePaths { original, paths } => {
                anyhow::ensure!(
                    self.same_origin_as(original.storage()),
                    "filesystem intent changed its physical owner"
                );
                let row = instances
                    .iter()
                    .find(|row| row.id == original.session_id())
                    .context("filesystem intent owner disappeared")?;
                original.validate_baseline_at(row, original.generation())?;
                anyhow::ensure!(row.lifecycle_reservation.as_ref().is_some_and(|lease| {
                    lease.generation == original.generation()
                        && matches!(&lease.path_claims, super::WorktreePathClaims::Pending(stored) if stored.as_slice() == *paths)
                }), "filesystem intent is unknown or differs from its complete original plan");
                Some(original.session_id())
            }
            StorageWriteScope::CompleteCreation { original } => {
                anyhow::ensure!(
                    self.same_origin_as(original.storage()),
                    "creation changed its physical owner"
                );
                let row = instances
                    .iter()
                    .find(|row| row.id == original.session_id())
                    .context("creation filesystem owner disappeared")?;
                original.validate_row(row)?;
                Some(original.session_id())
            }
            _ => None,
        };
        let groups_before = groups.clone();
        let result = f(&mut instances, &mut groups)?;
        let metadata_only = matches!(scope, StorageWriteScope::Metadata(_));
        if metadata_only {
            anyhow::ensure!(
                instances.len() == before.len(),
                "metadata write changed row ownership"
            );
        }
        let mut seen = std::collections::HashSet::with_capacity(instances.len());
        let mut deltas = Vec::new();
        for row in &instances {
            anyhow::ensure!(
                seen.insert(row.id.as_str()),
                "write duplicated a session owner"
            );
            let prior = before.get(&row.id);
            if metadata_only
                || prior.is_some_and(|prior| {
                    prior.pending.is_some() && completing != Some(row.id.as_str())
                })
            {
                let prior = prior.context("metadata write introduced a session owner")?;
                anyhow::ensure!(prior.matches(row), "write changed protected row geometry");
                if let Some(pending) = &prior.pending {
                    anyhow::ensure!(
                        row.lifecycle_reservation.as_ref() == Some(pending),
                        "write changed protected filesystem intent"
                    );
                }
            }
            let changed_geometry = prior.is_none_or(|prior| !prior.matches(row));
            let pending = row
                .lifecycle_reservation
                .as_ref()
                .filter(|lease| lease.path_claims.is_pending());
            let changed_pending = pending != prior.and_then(|prior| prior.pending.as_ref());
            if prior.is_none() || changed_geometry || changed_pending {
                super::retained_intents::ensure_id_available(&row.id)?;
            }
            if metadata_only {
                anyhow::ensure!(
                    !changed_geometry && !changed_pending,
                    "metadata write changed geometry or filesystem intent"
                );
            } else if changed_geometry
                || changed_pending && (pending.is_some() || completing == Some(row.id.as_str()))
            {
                let mut paths = row
                    .durable_worktree_paths()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                match pending.map(|lease| &lease.path_claims) {
                    Some(super::WorktreePathClaims::Pending(pending)) => {
                        paths.extend(pending.iter().cloned())
                    }
                    Some(super::WorktreePathClaims::Unknown(_)) => {
                        anyhow::bail!("cannot publish an unknown filesystem intent")
                    }
                    _ => {}
                }
                deltas.push((row.id.as_str(), paths));
            }
        }
        for (id, prior) in &before {
            if metadata_only || (prior.pending.is_some() && completing != Some(id.as_str())) {
                anyhow::ensure!(
                    seen.contains(id.as_str()),
                    "write removed a filesystem intent owner"
                );
            }
        }
        if !deltas.is_empty() {
            let owned;
            let (claims, profile) = if let StorageWriteScope::FencedGeometry(claims) = &scope {
                (*claims, claims.writer_profile(self)?)
            } else {
                owned =
                    super::deletion::PathClaimIndex::load_for_writer(std::slice::from_ref(self))?;
                let profile = owned.writer_profile(self)?;
                (&owned, profile)
            };
            for (id, paths) in deltas {
                claims.ensure_pending_unclaimed(profile, id, &paths)?;
            }
        }

        // Compose both documents before the first write, retaining opaque rows.
        let instances_buf = session_document.render(
            &instances,
            |row| row.id.as_str(),
            selected.as_ref().map(|selected| &selected.sessions),
            HashMap::new(),
        )?;
        groups.retain(|group| {
            !group_document.owners.contains_key(&group.path)
                || group_document.baseline(&group.path).is_some()
        });
        let groups_changed = groups != groups_before;
        let groups_buf = if groups_changed {
            Some(group_document.render(
                &groups,
                |group| group.path.as_str(),
                selected.as_ref().map(|selected| &selected.groups),
                HashMap::new(),
            )?)
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
    /// The caller owns workspace before identity and per-row locks.
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
    /// The caller owns workspace before identity and per-row locks.
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
        with_two_storage_locks(self, target, || {
            let source_groups_path = self.sessions_path.with_file_name("groups.json");
            let target_groups_path = target.sessions_path.with_file_name("groups.json");
            if existing_paths_share_filesystem_identity(&self.sessions_path, &target.sessions_path)?
            {
                return Err(anyhow!(
                    "source and target profiles resolve to the same physical sessions file"
                ));
            }
            if existing_paths_share_filesystem_identity(&source_groups_path, &target_groups_path)? {
                return Err(anyhow!(
                    "source and target profiles resolve to the same physical groups file"
                ));
            }

            let (source_document, mut source_instances) = self.load_projected_sessions_locked()?;
            let (target_document, mut target_instances) =
                target.load_projected_sessions_locked()?;
            source_document.admit_all()?;
            target_document.admit_all()?;
            let (source_group_document, mut source_groups) =
                super::raw_document::RowDocument::project::<Group>(
                    self.load_raw_document_locked(&source_groups_path)?,
                    "path",
                )?;
            let (target_group_document, mut target_groups) =
                super::raw_document::RowDocument::project::<Group>(
                    target.load_raw_document_locked(&target_groups_path)?,
                    "path",
                )?;
            source_group_document.admit_all()?;
            target_group_document.admit_all()?;
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
                anyhow::ensure!(
                    !source.has_pending_worktree_path_claims(),
                    "cannot move a filesystem intent owner between profiles"
                );
                let mut candidate = source.clone();
                if plan.merge_complete_post {
                    candidate.merge_profile_move_diff(before, after, plan.account_swap);
                } else {
                    candidate.merge_user_action_diff(before, after);
                }
                candidate.source_profile.clone_from(&target.profile);
                anyhow::ensure!(
                    !candidate.has_pending_worktree_path_claims(),
                    "profile move introduced a filesystem intent"
                );
                moved.push(candidate);
            }
            let claims =
                super::deletion::PathClaimIndex::load_for_writer(&[self.clone(), target.clone()])?;
            let source_profile = claims.writer_profile(self)?;
            let target_profile = claims.writer_profile(target)?;
            for (before, candidate) in changes.iter().map(|(before, _)| before).zip(&moved) {
                let source = source_instances
                    .iter()
                    .find(|row| row.id == before.id)
                    .context("source owner disappeared before profile move")?;
                let source_paths = source
                    .durable_worktree_paths()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let target_paths = candidate
                    .durable_worktree_paths()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                claims.ensure_pending_unclaimed(source_profile, &source.id, &source_paths)?;
                claims.ensure_pending_unclaimed(target_profile, &candidate.id, &target_paths)?;
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

            let source_groups_before = serde_json::to_vec_pretty(&source_group_document.raw.rows)?;
            let target_instances_before = serde_json::to_vec_pretty(&target_document.raw.rows)?;
            let target_groups_before = serde_json::to_vec_pretty(&target_group_document.raw.rows)?;
            let source_instances_before = serde_json::to_vec_pretty(&source_document.raw.rows)?;
            source_instances.retain(|instance| !ids.contains(instance.id.as_str()));
            target_instances.extend(moved.iter().cloned());
            apply_group_move(
                plan.group_move,
                &source_instances,
                &mut source_groups,
                &target_instances,
                &mut target_groups,
            );
            let source_instances_after = source_document.render(
                &source_instances,
                |row| row.id.as_str(),
                None,
                HashMap::new(),
            )?;
            let source_groups_after = source_group_document.render(
                &source_groups,
                |row| row.path.as_str(),
                None,
                HashMap::new(),
            )?;
            let mut imported_rows = HashMap::with_capacity(moved.len());
            for row in &moved {
                imported_rows.insert(row.id.as_str(), source_document.compose_row(&row.id, row)?);
            }
            let target_instances_after = target_document.render(
                &target_instances,
                |row| row.id.as_str(),
                None,
                imported_rows,
            )?;
            let mut imported_groups = HashMap::new();
            if plan.group_move.move_subtree
                || plan.group_move.source_path == plan.group_move.target_path
            {
                for group in &target_groups {
                    if target_group_document.owners.contains_key(&group.path) {
                        continue;
                    }
                    if let Some(suffix) = group
                        .path
                        .strip_prefix(&plan.group_move.target_path)
                        .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                    {
                        let source_path = format!("{}{suffix}", plan.group_move.source_path);
                        if source_group_document.owners.contains_key(&source_path) {
                            imported_groups.insert(
                                group.path.as_str(),
                                source_group_document.compose_row(&source_path, group)?,
                            );
                        }
                    }
                }
            }
            let target_groups_after = target_group_document.render(
                &target_groups,
                |row| row.path.as_str(),
                None,
                imported_groups,
            )?;
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
                        if moved.iter().all(|candidate| {
                            instances.iter().all(|row| row.id != candidate.id)
                        }) =>
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
            let origin = Arc::new(target.clone());
            for row in &mut moved {
                row.storage_origin = Some(origin.clone());
            }
            Ok(moved)
        })
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

/// Hold every distinct save mutex and physical flock until the operation returns.
pub(crate) fn with_storages_locked<R>(storages: &[Storage], f: impl FnOnce() -> R) -> Result<R> {
    let mut logical_dirs = Vec::with_capacity(storages.len());
    let mut sorted = storages
        .iter()
        .map(|storage| {
            let dir = storage
                .sessions_path
                .parent()
                .context("sessions path has no parent")?;
            logical_dirs.push(dir);
            Ok((dir.canonicalize()?, storage))
        })
        .collect::<Result<Vec<_>>>()?;
    sorted.sort_unstable_by_key(|(_, storage)| Arc::as_ptr(&storage.save_lock));
    let mut previous = None;
    let _mutexes: Vec<_> = sorted
        .iter()
        .map(|(_, storage)| *storage)
        .filter(|storage| {
            let key = Arc::as_ptr(&storage.save_lock);
            let distinct = previous != Some(key);
            previous = Some(key);
            distinct
        })
        .map(|storage| {
            storage
                .save_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        })
        .collect();
    sorted.sort_by(|(left, _), (right, _)| left.cmp(right));
    sorted.dedup_by(|(left, _), (right, _)| left == right);
    let _transition_flocks = acquire_transition_flocks_for_profile_dirs(&logical_dirs)?;
    let files = sorted
        .iter()
        .map(|(dir, _)| {
            let (file, path) = open_existing_storage_lock_file(dir, STORAGE_LOCK_FILENAME)?;
            OpenStorageLock::new(file, path)
        })
        .collect::<Result<Vec<_>>>()?;
    let _flocks = acquire_open_storage_flocks(files, false)?;
    Ok(f())
}

fn with_two_storage_locks<F, R>(source: &Storage, target: &Storage, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    let source_parent = source
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("source sessions path has no parent"))?;
    let target_parent = target
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("target sessions path has no parent"))?;
    let source_dir = source_parent.canonicalize()?;
    let target_dir = target_parent.canonicalize()?;
    if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
        anyhow::bail!("source and target resolve to the same physical profile directory");
    }
    let _mutexes = lock_two_save_mutexes(source, target);
    let _transition_flocks =
        acquire_transition_flocks_for_profile_dirs(&[source_parent, target_parent])?;
    let (source_file, source_path) =
        open_existing_storage_lock_file(&source_dir, STORAGE_LOCK_FILENAME)?;
    let (target_file, target_path) =
        open_existing_storage_lock_file(&target_dir, STORAGE_LOCK_FILENAME)?;
    let source_file = OpenStorageLock::new(source_file, source_path)?;
    let target_file = OpenStorageLock::new(target_file, target_path)?;
    anyhow::ensure!(
        source_file.physical_key() != target_file.physical_key(),
        "source and target resolve to the same physical storage lock"
    );
    let (first, second) = if source_file.physical_key() < target_file.physical_key() {
        (source_file, target_file)
    } else {
        (target_file, source_file)
    };
    let _first_flock = first.acquire(false)?;
    let _second_flock = second.acquire(false)?;
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

    let _namespace = acquire_profile_namespace_lock()?;
    source_storage.verify_profile_identity()?;
    target_storage.verify_profile_identity()?;
    with_two_storage_locks(source_storage, target_storage, || {
        let source_instances = source_storage.load_strict_for_worktree_ownership_locked()?;
        let target_instances = target_storage.load_strict_for_worktree_ownership_locked()?;
        if source_instances
            .iter()
            .chain(&target_instances)
            .any(|row| entry.ids.contains(&row.id) && row.has_pending_worktree_path_claims())
        {
            return Ok(false);
        }
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
        }, StorageWriteScope::Geometry)?;
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

/// Ring budget per marker, counting the copy the call just made.
const BACKUPS_TO_KEEP: usize = 3;
const _: () = assert!(
    BACKUPS_TO_KEEP > 1,
    "a ring must hold a copy and its predecessor"
);

/// Marker of the copies the move-journal repair takes. Renaming it would
/// orphan every `.pre-recovery-` already on disk, which no longer counts as a
/// copy to keep or to prune.
const REPAIR_BACKUP_MARKER: &str = ".pre-recovery-";

/// Marker of the copies a retype migration takes. A downgrade is served from
/// these and from nothing else, so they keep a ring of their own: a repair's
/// own copies are interchangeable with each other, these are not.
pub(crate) const MIGRATION_BACKUP_MARKER: &str = ".pre-migration-";

/// Back up a file the move-journal repair is about to rewrite. `Err` means no
/// durable copy landed, and the caller is mid-repair, so it must stop. A `path`
/// holding nothing to copy is not a failure: the repair backs up a
/// `groups.json` that many profiles never had.
fn backup_before_repair(path: &Path) -> Result<()> {
    let Some(backup) = backup_beside(path, REPAIR_BACKUP_MARKER)? else {
        return Ok(());
    };
    sync_parent_directory(&backup)
        .with_context(|| format!("backup {} was not made durable", backup.display()))
}

/// Back up a file a retype migration is about to rewrite, so a build that
/// predates the new shape still has bytes it can read. `Err` means no copy
/// landed, and the migration chooses the rewrite anyway rather than leave the
/// data half-migrated. A copy that landed without its directory entry made
/// durable is logged rather than returned, for the same reason. Nothing at
/// `path` is not a failure either: there is no rewrite to protect.
pub(crate) fn backup_before_migration(path: &Path) -> Result<()> {
    let Some(backup) = backup_beside(path, MIGRATION_BACKUP_MARKER)? else {
        return Ok(());
    };
    if let Err(error) = sync_parent_directory(&backup)
        .with_context(|| format!("backup {} was not made durable", backup.display()))
    {
        tracing::warn!(target: "session.store", %error, path = %backup.display(), "migration backup not synced into its directory");
    }
    Ok(())
}

/// Write one copy beside `path` under `marker`, then prune the set that was
/// there before, never the copy just made. `None` means `path` held nothing to
/// copy.
fn backup_beside(path: &Path, marker: &str) -> Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context(format!("failed reading {} for backup", path.display()))
        }
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("a file to back up needs a name: {}", path.display()))?;
    let backups = backups_beside(path, marker)
        .with_context(|| format!("failed listing backups beside {}", path.display()))?;
    // Above every sibling while the stamp space leaves room, so a copy taken in
    // the same millisecond as the last one cannot land on it. Only `u128::MAX`
    // leaves no room above; the search then takes a free stamp at the clock,
    // which sorts below that sibling.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let newest = backups.last().map_or(0, |(stamp, _)| *stamp);
    let mut stamp = now.max(newest.saturating_add(1));
    if stamp <= newest {
        stamp = now;
        // n entries can cover the n candidates now..now+n, so the next one is
        // free, unless the counter saturates and stops making progress.
        for _ in 0..backups.len() {
            if !backups.iter().any(|(taken, _)| *taken == stamp) {
                break;
            }
            let Some(next) = stamp.checked_add(1) else {
                return Err(anyhow!("recovery backup stamp space exhausted"));
            };
            stamp = next;
        }
    }
    // Not `atomic_write_verified`: that resolves the name first, so a link
    // planted at it between the scan and the write sends the copy outside the
    // profile directory. This renames inside the directory it already opened.
    let mut name = file_name.to_os_string();
    name.push(format!("{marker}{stamp}"));
    let backup = path.with_file_name(&name);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("a file to back up needs a parent: {}", path.display()))?;
    replace_file_no_follow_as(parent, Path::new(&name), &bytes, 0o600)?;
    // The copy is on disk, so what remains only tidies the siblings. A prune
    // that cannot run is worth a line, not a failed backup.
    if let Err(error) = prune_backups_beside(path, &backups, sync_resolved_parent_directory) {
        tracing::warn!(target: "session.store", %error, path = %path.display(), "backups not pruned");
    }
    Ok(Some(backup))
}

/// The timestamped backups `backup_beside` would write beside `path`, oldest
/// first. Entries that are not regular files stay in: their name must stay
/// unchoosable, and the prune skips what it cannot remove.
fn backups_beside(path: &Path, marker: &str) -> Result<Vec<(u128, PathBuf)>> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("a file to back up needs a parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("a file to back up needs a name: {}", path.display()))?;
    // Matched on the raw bytes the writer will use, so a non-UTF-8 name scans
    // the copies it actually made instead of silently finding none.
    let mut prefix = file_name.to_os_string();
    prefix.push(marker);
    let mut backups: Vec<(u128, PathBuf)> = fs::read_dir(parent)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|candidate| {
            let name = candidate.file_name()?;
            let stamp = name
                .as_encoded_bytes()
                .strip_prefix(prefix.as_encoded_bytes())?;
            let timestamp = std::str::from_utf8(stamp).ok()?.parse::<u128>().ok()?;
            Some((timestamp, candidate))
        })
        .collect();
    backups.sort_by_key(|(timestamp, _)| *timestamp);
    Ok(backups)
}

/// Drop the oldest of `backups` that do not fit [`BACKUPS_TO_KEEP`] counting
/// the copy the caller is about to make, which is why `backups` must be the
/// pre-write scan. An entry that cannot be removed spends one of those slots
/// and is stepped over, so the loop stays bounded; the directory then holds
/// one file more per entry it could not remove.
fn prune_backups_beside<S>(path: &Path, backups: &[(u128, PathBuf)], mut sync: S) -> Result<()>
where
    S: FnMut(&Path) -> Result<()>,
{
    let mut remaining = backups.len();
    let mut pruned = 0usize;
    for (_, oldest) in backups {
        if remaining < BACKUPS_TO_KEEP {
            break;
        }
        remaining -= 1;
        match fs::remove_file(oldest) {
            Ok(()) => pruned += 1,
            Err(error) => {
                tracing::warn!(target: "session.store", %error, path = %oldest.display(), "old backup not pruned")
            }
        }
    }
    if pruned > 0 {
        // A migration ring's oldest copy is the one a downgrade is served from,
        // and a repair ring's is the one it is not, so name the ring rather than
        // claim a reason: nothing else in the app reports that a prune happened.
        tracing::info!(target: "session.store", pruned, path = %path.display(), "old backups pruned");
    }
    // Backup files are lexical siblings of path even when path itself is a symlink.
    sync(path).context("backup pruning was not made durable")
}

#[cfg(test)]
fn repair_backups(path: &Path) -> Result<Vec<(u128, PathBuf)>> {
    backups_beside(path, REPAIR_BACKUP_MARKER)
}

#[cfg(test)]
pub(crate) fn migration_backups(path: &Path) -> Result<Vec<(u128, PathBuf)>> {
    backups_beside(path, MIGRATION_BACKUP_MARKER)
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
    #[test]
    #[serial_test::serial]
    fn crash_claim_reconciliation_is_durable_and_never_expires_or_erases_paths() -> Result<()> {
        use crate::process::{CustodianLiveness, OriginalCustodianBirth, ProcessIncarnation};
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("crash-claims")?;
        let root = tempfile::tempdir()?;
        let paths = vec![
            root.path().join("future"),
            root.path().join("second"),
            root.path().join("future"),
        ];
        let mut row = Instance::new("original", root.path().join("current").to_str().unwrap());
        row.try_acquire_lifecycle_reservation(
            super::super::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        let birth = OriginalCustodianBirth {
            boot: "fixture-boot".into(),
            process: ProcessIncarnation {
                pid: 4312,
                group: 4312,
                start: [123, 0],
                namespace: [7, 8],
            },
            profile: storage.original_profile_identity()?,
            session_id: row.id.clone(),
            created_at: row.created_at,
            generation: row.lifecycle_generation,
        };
        let lease = row.lifecycle_reservation.as_mut().unwrap();
        lease.path_claims = super::super::WorktreePathClaims::Pending(paths.clone());
        lease.custodian = Some(birth.clone());
        let raw = serde_json::to_string(&row)?;
        let opaque =
            r#"{"id":"opaque","project_path":false,"literal":{"same":1,"same":2,"number":1e400}}"#;
        let duplicated = r#"{"id":"duplicate","id":"duplicate","project_path":"/tmp/keep"}"#;
        let bytes = format!("[{raw},{opaque},{duplicated}]");
        fs::write(storage.sessions_path(), &bytes)?;
        for observation in [CustodianLiveness::Live, CustodianLiveness::Uncertain] {
            assert!(!storage.reconcile_filesystem_claims_with(|_| observation)?);
            assert_eq!(fs::read_to_string(storage.sessions_path())?, bytes);
        }
        let mut failed = storage.clone();
        failed.fail_writes_for_test = true;
        assert!(failed
            .reconcile_filesystem_claims_with(|_| CustodianLiveness::Lost)
            .is_err());
        assert_eq!(fs::read_to_string(storage.sessions_path())?, bytes);
        assert!(storage.reconcile_filesystem_claims_with(|_| CustodianLiveness::Lost)?);
        let persisted = fs::read(storage.sessions_path())?;
        let document =
            super::super::raw_document::RawDocument::parse(std::str::from_utf8(&persisted)?)?;
        let mut retained: Instance = serde_json::from_str(document.rows[0].get())?;
        assert_eq!(retained.id, row.id);
        assert_eq!(retained.created_at, row.created_at);
        assert_eq!(retained.lifecycle_generation, row.lifecycle_generation);
        let lease = retained.lifecycle_reservation.as_ref().unwrap();
        assert_eq!(lease.op, row.lifecycle_reservation.as_ref().unwrap().op);
        assert_eq!(lease.at, row.lifecycle_reservation.as_ref().unwrap().at);
        assert_eq!(lease.custodian.as_ref(), Some(&birth));
        assert_eq!(
            lease.path_claims,
            super::super::WorktreePathClaims::Unknown(Some(paths))
        );
        assert_eq!(document.rows[1].get(), opaque);
        assert_eq!(document.rows[2].get(), duplicated);
        let reopened = Storage::open_unwatched("crash-claims")?;
        assert!(!reopened
            .reconcile_filesystem_claims_with(|_| panic!("Unknown never becomes new authority"))?);
        assert_eq!(fs::read(reopened.sessions_path())?, persisted);
        let operation = row.lifecycle_reservation.as_ref().unwrap().op;
        assert!(
            !retained.release_lifecycle_reservation_if_owned(operation, row.lifecycle_generation)
        );
        assert!(retained
            .has_active_lifecycle_reservation(chrono::Utc::now() + chrono::Duration::days(90)));
        assert!(retained
            .try_acquire_lifecycle_reservation(
                super::super::LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now() + chrono::Duration::days(90)
            )
            .is_err());
        Ok(())
    }

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

    /// A row resolves its agent against the profile it was loaded from, and
    /// that profile is never persisted, so both load paths must stamp it.
    #[tokio::test]
    #[serial]
    async fn load_stamps_the_profile_each_row_came_from() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let svc = FileWatchService::new().expect("live svc");
        for profile in ["stamp-alpha", "stamp-beta"] {
            Storage::new(profile, svc.clone())?.update(|instances, _groups| {
                let mut row = Instance::new(&format!("row in {profile}"), "/tmp/seed");
                row.agent_session_id = Some("conversation-uuid".to_string());
                *instances = vec![row];
                Ok(())
            })?;
        }

        let (loaded, groups) = Storage::new("stamp-beta", svc.clone())?.load_with_groups()?;
        assert!(
            groups.is_empty(),
            "the fixture writes no groups, so a group here would mean the test read the wrong rows"
        );
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].source_profile, "stamp-beta");
        assert_eq!(loaded[0].effective_profile(), "stamp-beta");

        let plain = Storage::new("stamp-alpha", svc)?.load()?;
        assert_eq!(plain[0].source_profile, "stamp-alpha");
        Ok(())
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
    fn selected_geometry_refuses_unknown_absence_before_mutation() -> Result<()> {
        use crate::session::claim::{decide_purge_claim, PurgeClaimDecision};
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("selected-geometry")?;
        let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
        let owner = Instance::new("owner", temp.path().join("owner").to_str().unwrap());
        let owner_json = serde_json::to_string(&owner)?;
        let mut corrupt = serde_json::to_value(&owner)?;
        corrupt["title"] = serde_json::json!(false);
        let duplicate_field = format!("{},\"id\":\"other\"}}", &owner_json[..owner_json.len() - 1]);
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let groups = br#"[ {"path":"group","name":"group","expanded":true,"order":0,"extension":{"same":1,"same":2,"number":1e400}} ]"#;
        fs::write(&groups_path, groups)?;
        for (original, selected) in [
            (format!("[{owner_json},{owner_json}]"), owner.id.as_str()),
            (
                format!("[{}]", serde_json::to_string(&corrupt)?),
                owner.id.as_str(),
            ),
            (format!("[{duplicate_field}]"), owner.id.as_str()),
            (format!("[{duplicate_field}]"), "other"),
            (format!("[{owner_json},null]"), "missing"),
            (format!("[{owner_json},{owner_json}]"), "missing"),
            (r#"[{"id":false,"title":"unindexed"}]"#.into(), "missing"),
        ] {
            fs::write(storage.sessions_path(), original.as_bytes())?;
            let called = std::cell::Cell::new(false);
            let result =
                storage.update_owner_under_workspace_claim_lock(selected, |rows, groups| {
                    called.set(true);
                    rows.clear();
                    groups.clear();
                    Ok(())
                });
            assert!(result.is_err(), "{selected}: {original}");
            assert!(
                !called.get(),
                "raw admission must precede both mutation and absence"
            );
            assert_eq!(fs::read(storage.sessions_path())?, original.as_bytes());
            assert_eq!(fs::read(&groups_path)?, groups);
        }
        // Read diagnostics deliberately retain every decodable duplicate.
        fs::write(
            storage.sessions_path(),
            format!("[{owner_json},{owner_json}]"),
        )?;
        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, owner.id);
        assert_eq!(loaded[1].id, owner.id);
        fs::write(storage.sessions_path(), "[]")?;
        let absent = storage.update_owner_under_workspace_claim_lock("missing", |rows, _| {
            Ok(decide_purge_claim(
                rows,
                "missing",
                false,
                chrono::Utc::now(),
            )?)
        })?;
        assert_eq!(absent, PurgeClaimDecision::AlreadyGone);
        // An admitted owner can still coexist with unrelated frozen raw rows.
        fs::write(storage.sessions_path(), format!("[{owner_json},null]"))?;
        storage.update_owner_under_workspace_claim_lock(&owner.id, |rows, _| {
            rows[0].pin();
            Ok(())
        })?;
        let output = super::super::raw_document::RawDocument::parse(&fs::read_to_string(
            storage.sessions_path(),
        )?)?;
        assert_eq!(output.rows[1].get(), "null");
        assert!(serde_json::from_str::<Instance>(output.rows[0].get())?.is_pinned());
        assert_eq!(fs::read(&groups_path)?, groups);
        Ok(())
    }

    #[test]
    #[serial]
    fn ambiguous_nested_survivor_refuses_before_either_document_write() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("array-ambiguity")?;
        let mut owner = Instance::new("owner", temp.path().join("workspace").to_str().unwrap());
        let repo = crate::session::WorkspaceRepo {
            name: "repo".into(),
            source_path: "/source".into(),
            branch: "branch".into(),
            worktree_path: "/workspace/repo".into(),
            main_repo_path: "/source".into(),
            managed_by_aoe: false,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        };
        owner.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "branch".into(),
            workspace_dir: owner.project_path.clone(),
            repos: vec![repo.clone(), repo],
            created_at: owner.created_at,
            cleanup_on_delete: true,
        });
        let mut value = serde_json::to_value(&owner)?;
        value["workspace_info"]["repos"][0]["extension"] = serde_json::json!("first");
        value["workspace_info"]["repos"][1]["extension"] = serde_json::json!("second");
        let original = format!("[ {} ]", serde_json::to_string(&value)?);
        fs::write(storage.sessions_path(), &original)?;
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let groups = b"[ ]\n";
        fs::write(&groups_path, groups)?;
        let called = std::cell::Cell::new(false);
        let result = storage.update_metadata(
            MetadataSelection::Session(owner.id.as_str().into()),
            |rows, groups| {
                called.set(true);
                rows[0].workspace_info.as_mut().unwrap().repos[1].base_branch_override =
                    Some("main".into());
                groups.push(crate::session::Group::new("changed", "changed"));
                Ok(())
            },
        );
        assert!(
            called.get(),
            "exercise render refusal, not a pre-closure refusal"
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ambiguous canonical array member"));
        assert_eq!(fs::read(storage.sessions_path())?, original.as_bytes());
        assert_eq!(fs::read(&groups_path)?, groups);
        Ok(())
    }

    #[test]
    #[serial]
    fn scoped_metadata_preserves_opaque_rows_and_refuses_target_ambiguity() -> Result<()> {
        use super::super::raw_document::{RawDocument, RawObject};
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("raw-metadata")?;
        let mut healthy = Instance::new("healthy", temp.path().join("healthy").to_str().unwrap());
        healthy.group_path = "opaque".into();
        let mut corrupt = serde_json::to_value(Instance::new("corrupt", "/tmp/corrupt"))?;
        corrupt["title"] = serde_json::json!(17);
        let duplicate = Instance::new("duplicate", "/tmp/duplicate");
        let extension = r#"{"same":1,"same":2,"huge":1234567890123456789012345678901234567890,"float":1e400,"escaped":"\u0061"}"#;
        let healthy_json = serde_json::to_string(&healthy)?;
        let healthy_json = format!(
            "{},\"extension\":{extension}}}",
            &healthy_json[..healthy_json.len() - 1]
        );
        let corrupt_json = serde_json::to_string(&corrupt)?;
        let duplicate_json = serde_json::to_string(&duplicate)?;
        let original =
            format!("[{healthy_json},{corrupt_json},{duplicate_json},{duplicate_json},null]");
        fs::write(&storage.sessions_path, &original)?;
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let opaque_group = format!(r#"{{"path":"opaque","name":7,"extension":{extension}}}"#);
        fs::write(&groups_path, format!("[{opaque_group}]"))?;
        storage.update_metadata(
            MetadataSelection::Session(healthy.id.as_str().into()),
            |rows, _| {
                rows.iter_mut()
                    .find(|row| row.id == healthy.id)
                    .unwrap()
                    .pin();
                Ok(())
            },
        )?;
        assert!(storage
            .load()?
            .iter()
            .find(|row| row.id == healthy.id)
            .unwrap()
            .is_pinned());
        let after = RawDocument::parse(&fs::read_to_string(&storage.sessions_path)?)?;
        assert_eq!(
            RawObject::parse(&after.rows[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            extension
        );
        for (index, expected) in [
            (1, corrupt_json.as_str()),
            (2, duplicate_json.as_str()),
            (3, duplicate_json.as_str()),
            (4, "null"),
        ] {
            assert_eq!(after.rows[index].get(), expected);
        }
        let ids = [
            healthy.id.clone(),
            corrupt["id"].as_str().unwrap().to_owned(),
        ];
        for selection in [
            MetadataSelection::Sessions((&ids[..]).into()),
            MetadataSelection::Session(duplicate.id.as_str().into()),
            MetadataSelection::AllSessions,
            MetadataSelection::Group("opaque".into()),
        ] {
            let before_sessions = fs::read(&storage.sessions_path)?;
            let before_groups = fs::read(&groups_path)?;
            let called = std::cell::Cell::new(false);
            assert!(storage
                .update_metadata(selection, |rows, _| {
                    called.set(true);
                    rows[0].unpin();
                    Ok(())
                })
                .is_err());
            assert!(
                !called.get(),
                "ambiguous targets must refuse before any mutation"
            );
            assert_eq!(fs::read(&storage.sessions_path)?, before_sessions);
            assert_eq!(fs::read(&groups_path)?, before_groups);
        }
        storage.update_metadata(MetadataSelection::Group("new".into()), |rows, groups| {
            let mut tree = GroupTree::new_with_groups(rows, groups);
            tree.create_group("new");
            *groups = tree.get_all_groups();
            Ok(())
        })?;
        let groups = RawDocument::parse(&fs::read_to_string(&groups_path)?)?;
        assert_eq!(groups.rows[0].get(), opaque_group);
        assert!(groups.rows.iter().any(|raw| RawObject::parse(raw)
            .is_ok_and(|row| row.strings("path").any(|path| path == "new"))));
        Ok(())
    }

    #[test]
    #[serial]
    fn execution_store_replacement_and_reset_ack_keep_opaque_evidence() -> Result<()> {
        use super::super::raw_document::{RawDocument, RawObject};
        use crate::migrations::v033_isolate_sandbox_content::{
            acknowledge_context_reset, NativeContextView,
        };
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("canonical-context-update")?;
        let mut row = Instance::new("context-update", "/tmp/context-update");
        row.lifecycle_generation = 7;
        row.active_execution = Some(super::super::instance::ActiveExecution {
            launch_id: "original-execution".into(),
            binding: super::super::ExecutionBinding {
                agent: "pi".into(),
                stores: vec!["/tmp/original-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp/context-update".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            },
            capture: None,
            container: None,
        });
        let opaque = r#"{"same":1,"same":2,"float":1e400,"escaped":"\u0061"}"#;
        let reset = format!(
            r#"{{"slot":"original-slot","transaction":"original-transaction","tool":"{}","agent":"claude","roots":["/tmp/native-root"],"recovery":["/tmp/original-recovery"],"terminal":{{"pending":true,"generation":null}},"structured":{{"pending":true,"generation":7}},"retired_terminal":"terminal-context","retired_structured":["old-acp-context"],"retired_import":true,"extension":{opaque}}}"#,
            row.tool,
        );
        row.sandbox_content_resets
            .push(serde_json::from_str(&reset)?);
        let typed_reset = serde_json::to_string(&row.sandbox_content_resets[0])?;
        let binding = serde_json::to_string(&row.active_execution.as_ref().unwrap().binding)?;
        let raw_binding = format!("{},\"extension\":{opaque}}}", &binding[..binding.len() - 1]);
        let original = serde_json::to_string(&row)?
            .replace(&typed_reset, &reset)
            .replace(&binding, &raw_binding);
        fs::write(&storage.sessions_path, format!("[{original}]"))?;

        storage.update_metadata(
            MetadataSelection::Session(row.id.as_str().into()),
            |rows, _| {
                let execution = rows[0].active_execution.as_mut().unwrap();
                execution.launch_id = "next-execution".into();
                execution.binding.stores = vec!["/tmp/replacement-store".into()];
                Ok(())
            },
        )?;
        acknowledge_context_reset(
            storage.profile(),
            &row.id,
            NativeContextView::Structured,
            7,
            &["original-slot".into()],
            Some("fresh-acp-context"),
        )?;
        let bytes = fs::read(&storage.sessions_path)?;
        let document = RawDocument::parse(std::str::from_utf8(&bytes)?)?;
        let object = RawObject::parse(&document.rows[0])?;
        let execution = RawObject::parse(object.unique("active_execution")?.unwrap())?;
        let binding = RawObject::parse(execution.unique("binding")?.unwrap())?;
        assert_eq!(binding.unique("extension")?.unwrap().get(), opaque);
        let resets: Vec<&serde_json::value::RawValue> =
            serde_json::from_str(object.unique("sandbox_content_resets")?.unwrap().get())?;
        assert_eq!(
            RawObject::parse(resets[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            opaque
        );
        let stored = storage.load()?.remove(0);
        assert_eq!(stored.acp_session_id.as_deref(), Some("fresh-acp-context"));
        assert_eq!(
            stored.active_execution.unwrap().binding.stores,
            vec![PathBuf::from("/tmp/replacement-store")]
        );
        let reset_value = serde_json::to_value(&stored.sandbox_content_resets[0])?;
        assert_eq!(reset_value["structured"]["pending"], false);
        assert_eq!(reset_value["terminal"]["pending"], true);
        assert_eq!(
            reset_value["recovery"],
            serde_json::json!(["/tmp/original-recovery"])
        );

        // A shared immutable tuple must not choose which literal original survives.
        let duplicate_reset = reset.replace(opaque, r#"{"same":3,"same":4,"float":1e400}"#);
        let ambiguous = original.replace(&reset, &format!("{reset},{duplicate_reset}"));
        let ambiguous = format!("[{ambiguous}]");
        fs::write(&storage.sessions_path, &ambiguous)?;
        assert!(acknowledge_context_reset(
            storage.profile(),
            &row.id,
            NativeContextView::Structured,
            7,
            &["original-slot".into()],
            Some("must-not-publish"),
        )
        .is_err());
        assert_eq!(fs::read(&storage.sessions_path)?, ambiguous.as_bytes());
        Ok(())
    }

    #[test]
    #[serial]
    fn open_unwatched_requires_existing_profile() {
        let temp = tempdir().unwrap();
        let guard = setup_test_home(temp.path());
        let profile_dir = guard.path().join("profiles").join("ghost");

        let Err(err) = Storage::open_unwatched("ghost") else {
            panic!("opening an unknown profile must fail");
        };
        assert!(err.to_string().contains("does not exist"), "got: {err}");
        assert!(!profile_dir.exists(), "must not create the profile dir");

        crate::session::create_profile("known").unwrap();
        assert_eq!(Storage::open_unwatched("known").unwrap().profile(), "known");
    }

    #[test]
    #[serial]
    fn stale_storage_writer_cannot_recreate_renamed_or_deleted_profile() {
        let temp = tempdir().unwrap();
        let guard = setup_test_home(temp.path());
        crate::session::create_profile("old").unwrap();
        crate::session::create_profile("other").unwrap();
        let stale = Storage::open_unwatched("old").unwrap();
        crate::session::rename_profile("old", "renamed").unwrap();
        assert!(stale.update(|_, _| Ok(())).is_err());
        assert!(!guard.path().join("profiles/old").exists());

        let stale_delete = Storage::open_unwatched("other").unwrap();
        crate::session::delete_profile("other").unwrap();
        assert!(stale_delete.update(|_, _| Ok(())).is_err());
        assert!(!guard.path().join("profiles/other").exists());
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

    /// A non-empty quarantine sidecar is a write-only forensic artifact, never an
    /// input: once a later write dropped the corrupt row from `sessions.json`, the
    /// inventory on disk is fully readable and must be usable for a destructive
    /// ownership check.
    #[test]
    #[serial]
    fn ownership_inventory_ignores_a_stale_quarantine_sidecar() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        let sessions = serde_json::json!([
            Instance::new("alpha", "/tmp/alpha"),
            Instance::new("beta", "/tmp/beta"),
        ]);
        fs::write(&storage.sessions_path, serde_json::to_vec(&sessions)?)?;
        fs::write(
            storage
                .sessions_path
                .with_file_name("sessions.corrupt.jsonl"),
            "{\"title\":\"long-gone\"}\n",
        )?;

        let inventory = storage.load_strict_for_worktree_ownership_locked()?;
        let titles: Vec<_> = inventory.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["alpha", "beta"]);
        Ok(())
    }

    /// The fail-closed guarantee the sidecar check used to imply still holds where it
    /// counts: a row still sitting in `sessions.json` that will not deserialize.
    #[test]
    #[serial]
    fn ownership_inventory_still_refuses_a_corrupt_row_in_sessions_json() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        let sessions = serde_json::json!([
            Instance::new("alpha", "/tmp/alpha"),
            { "title": "corrupt-no-id" },
        ]);
        fs::write(&storage.sessions_path, serde_json::to_vec(&sessions)?)?;

        assert!(storage.load_strict_for_worktree_ownership_locked().is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn ownership_inventory_distinguishes_missing_file_from_broken_alias() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;
        assert!(storage
            .load_strict_for_worktree_ownership_locked()?
            .is_empty());
        let absent = temp.path().join("absent-inventory");
        std::os::unix::fs::symlink(&absent, &storage.sessions_path)?;
        assert!(storage.load_strict_for_worktree_ownership_locked().is_err());
        fs::write(
            &absent,
            serde_json::to_vec(&vec![Instance::new("peer", "/tmp/peer")])?,
        )?;
        let rows = storage.load_strict_for_worktree_ownership_locked()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "peer");
        fs::remove_file(&storage.sessions_path)?;
        assert!(storage
            .load_strict_for_worktree_ownership_locked()?
            .is_empty());
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

    /// A writer whose profile was deleted mid-flight must not bring it back: the
    /// lock file opened with `create_dir_all` resurrected an empty directory
    /// between the two identity checks, and `list_profiles` then reported a
    /// profile the user had removed.
    #[test]
    #[serial]
    fn a_superseded_writer_does_not_recreate_a_deleted_profile() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-superseded-writer")?;
        let profile_dir = storage.sessions_path.parent().unwrap().to_path_buf();
        // Holding the namespace transition flock parks the writer between its
        // two identity checks, which is the window the recreation lived in.
        let transition = acquire_storage_flock(
            app_dir_for_profile_dir(&profile_dir),
            crate::migrations::v027_isolate_sandbox_stores::LOCK,
        )?;
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _observer = observe_lock_contention_for_test(contended_tx);
            storage.update(|instances, _| {
                instances.push(Instance::new("late", "/tmp/late"));
                Ok(())
            })
        });
        contended_rx
            .recv_timeout(Duration::from_secs(2))
            .context("writer must reach the profile namespace transition lock")?;
        fs::remove_dir_all(&profile_dir)?;
        drop(transition);

        assert!(
            writer.join().unwrap().is_err(),
            "the write must fail once its profile namespace is gone"
        );
        assert!(
            !profile_dir.exists(),
            "the writer resurrected the deleted profile directory"
        );
        assert!(
            !crate::session::list_profiles()?.contains(&"test-superseded-writer".to_string()),
            "a resurrected profile must not be listed"
        );
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
        assert_eq!(
            storage.load()?.len(),
            2,
            "a failed write leaves disk unchanged"
        );

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

    /// A destructive reopen keeps the caller's watcher, so a write through the
    /// reopened store still reaches the live subscription instead of the noop
    /// service an unwatched reopen would wire.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn reopen_preserving_watch_keeps_the_callers_watcher() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let _watch_env = crate::session::test_support::EnvGuard::unset(&["AOE_FILE_WATCH"]);
        let svc = FileWatchService::new().expect("live svc");
        let storage = Storage::new("test-reopen-watch", svc.clone())?;
        storage.update(|instances, _groups| {
            *instances = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })?;

        let profile_dir = get_profile_dir("test-reopen-watch")?;
        let sessions_path = profile_dir.join("sessions.json");
        let (mut sessions_rx, _sessions_h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir,
                    matcher: FileMatcher::Exact(sessions_path),
                    debounce: None,
                },
                128,
            )
            .expect("subscribe sessions");

        let reopened = storage.reopen_preserving_watch()?;
        assert_eq!(reopened.profile(), storage.profile());
        reopened.update(|instances, _groups| {
            instances.push(Instance::new("published", "/tmp/published"));
            Ok(())
        })?;

        tokio::time::timeout(
            Duration::from_secs(2),
            crate::file_watch::test_support::dispatch_barrier(&svc),
        )
        .await?;
        let mut local = false;
        loop {
            match sessions_rx.try_recv() {
                Ok(event) => local |= event.source == crate::file_watch::EventSource::Local,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("live dispatcher closed")
                }
            }
        }
        assert!(
            local,
            "a write through the reopened store must reach the caller's watcher"
        );
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
    #[serial]
    fn profile_import_publishes_geometry_after_original_metadata_transaction() -> Result<()> {
        use super::super::raw_document::{RawDocument, RawObject};
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_unwatched("array-import-source")?;
        let target = Storage::new_unwatched("array-import-target")?;
        let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
        let _identity = crate::session::acquire_session_identity_lock()?;
        let mut before = Instance::new("owner", temp.path().join("old").to_str().unwrap());
        before.group_path = "old-group".into();
        before.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "old-branch".into(),
            main_repo_path: "/source".into(),
            managed_by_aoe: false,
            created_at: before.created_at,
            base_branch: None,
        });
        let extension = r#"{"same":1,"same":2,"number":1e400,"escaped":"\u0061"}"#;
        let row = serde_json::to_string(&before)?;
        let original = format!("[{},\"extension\":{extension}}}]", &row[..row.len() - 1]);
        fs::write(source.sessions_path(), &original)?;
        fs::write(
            source.sessions_path.with_file_name("groups.json"),
            serde_json::to_vec(&vec![Group::new("old-group", "old-group")])?,
        )?;
        let mut after = before.clone();
        after.project_path = temp.path().join("moved").to_str().unwrap().into();
        after.worktree_info.as_mut().unwrap().branch = "renamed-branch".into();
        after.group_path = "new-group".into();
        let effect_ran = std::cell::Cell::new(false);
        let committed = source.move_instance_to_with_effect(
            &target,
            &before,
            &after,
            false,
            |_, _| Ok(()),
            |candidate| {
                assert_eq!(candidate.id, before.id);
                assert_eq!(candidate.created_at, before.created_at);
                assert_eq!(candidate.project_path, after.project_path);
                assert_eq!(fs::read_to_string(source.sessions_path())?, original);
                effect_ran.set(true);
                Ok(())
            },
        )?;
        assert!(effect_ran.get());
        assert_eq!(committed.project_path, after.project_path);
        let output = RawDocument::parse(&fs::read_to_string(target.sessions_path())?)?;
        assert_eq!(output.rows.len(), 1);
        assert_eq!(
            RawObject::parse(&output.rows[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            extension
        );
        let published: Instance = serde_json::from_str(output.rows[0].get())?;
        assert_eq!(published.id, before.id);
        assert_eq!(published.created_at, before.created_at);
        assert_eq!(published.project_path, after.project_path);
        assert_eq!(published.worktree_info.unwrap().branch, "renamed-branch");
        assert!(
            RawDocument::parse(&fs::read_to_string(source.sessions_path())?)?
                .rows
                .is_empty()
        );
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
        let groups_path = source.sessions_path.with_file_name("groups.json");
        let groups_raw = fs::read_to_string(&groups_path)?;
        let group_raw: Vec<Box<serde_json::value::RawValue>> = serde_json::from_str(&groups_raw)?;
        let group_raw = group_raw[0].get();
        let extension = r#"{"key":1,"key":2,"number":1e400,"escaped":"\u0061"}"#;
        fs::write(
            &groups_path,
            format!(
                "[{},\"extension\":{extension}}}]",
                &group_raw[..group_raw.len() - 1]
            ),
        )?;

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
        let target_raw = super::super::raw_document::RawDocument::parse(&fs::read_to_string(
            target.sessions_path.with_file_name("groups.json"),
        )?)?;
        assert_eq!(
            super::super::raw_document::RawObject::parse(&target_raw.rows[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            extension
        );
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

            let (source_rows, target_rows) = (source.load()?, target.load()?);
            assert_eq!(source_rows.len(), 1, "{point}: source row remains");
            assert_eq!(target_rows.len(), 1, "{point}: target copy durable");
            assert_eq!(
                source_rows[0].id, target_rows[0].id,
                "{point}: duplicate id"
            );
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

    /// Journal evidence that cannot prove a winner (#3527) never arbitrates: every copy stays on
    /// disk and the duplicate stays surfaced. Rows collect failures so each guard reports alone.
    #[test]
    fn unprovable_journal_evidence_keeps_duplicates_surfaced() -> Result<()> {
        type Arrange = fn(&Storage, &Storage, &Instance) -> Result<()>;
        fn record(entry: &move_journal::MoveJournalEntry, at: &Storage) -> Result<()> {
            move_journal::record(entry, at.sessions_path()).map(drop)
        }
        fn copy_and_record(
            edit: fn(&mut move_journal::MoveJournalEntry),
        ) -> impl Fn(&Storage, &Storage, &Instance) -> Result<()> {
            move |a, b, before| {
                push_copy(b, before)?;
                let mut entry = fresh_journal_entry(a, b, &before.id);
                edit(&mut entry);
                record(&entry, a)
            }
        }
        let legacy: Arrange = |_, b, before| push_copy(b, before);
        let wrong_version: Arrange = |a, b, before| {
            copy_and_record(|e| e.version = move_journal::MOVE_JOURNAL_VERSION + 1)(a, b, before)
        };
        // Stores untouched since before the entry, so only its age disqualifies it.
        let expired: Arrange = |a, b, before| {
            copy_and_record(|e| e.created_at_epoch_ms = now_ms() - 8 * 24 * 3600 * 1000)(
                a, b, before,
            )?;
            let stale =
                std::time::UNIX_EPOCH + Duration::from_millis(now_ms() - 9 * 24 * 3600 * 1000);
            for storage in [a, b] {
                fs::File::options()
                    .write(true)
                    .open(storage.sessions_path())?
                    .set_modified(stale)?;
            }
            Ok(())
        };
        let edited_after: Arrange = |a, b, before| {
            copy_and_record(|e| e.created_at_epoch_ms = now_ms() - 10 * 60 * 1000)(a, b, before)
        };
        let invalid_id: Arrange =
            |a, b, before| copy_and_record(|e| e.ids = vec!["../escape".into()])(a, b, before);
        let duplicate_ids: Arrange = |a, b, before| {
            let mut entry = fresh_journal_entry(a, b, &before.id);
            entry.ids.push(before.id.clone());
            record(&entry, a)
        };
        let aliased: Arrange = |a, b, before| {
            let mut entry = fresh_journal_entry(a, b, &before.id);
            entry.target_profile = a.profile().to_string();
            entry.target_sessions_path = a.sessions_path().to_path_buf();
            record(&entry, a)
        };
        let target_duplicate: Arrange = |a, b, before| {
            fs::write(b.sessions_path(), serde_json::to_vec(&[before, before])?)?;
            record(&fresh_journal_entry(a, b, &before.id), a)
        };
        let same_profile: Arrange = |a, _, before| {
            fs::write(a.sessions_path(), serde_json::to_vec(&[before, before])?)?;
            Ok(())
        };
        let newer_unresolved: Arrange = |a, b, before| {
            push_copy(b, before)?;
            let mut older = fresh_journal_entry(a, b, &before.id);
            older.created_at_epoch_ms = now_ms() - 60_000;
            record(&older, a)?;
            let mut newer = fresh_journal_entry(b, a, &before.id);
            newer.target_profile = "missing-profile".to_string();
            newer.target_sessions_path = a.sessions_path().with_file_name("missing.json");
            record(&newer, b)
        };
        let newer_opaque: Arrange = |a, b, before| {
            push_copy(b, before)?;
            let mut older = fresh_journal_entry(a, b, &before.id);
            older.created_at_epoch_ms = now_ms() - 60_000;
            record(&older, a)?;
            let journal_dir = b.sessions_path().parent().unwrap().join(".move-journal");
            fs::create_dir_all(&journal_dir)?;
            fs::write(
                journal_dir.join("move-99999999999999999999-1.json"),
                b"not-json",
            )?;
            Ok(())
        };
        // A newer unresolvable X journal shadows the X+Y batch, which must in turn shadow the
        // older Y-only journal.
        let shadowed_batch: Arrange = |a, b, x| {
            let mut y = Instance::new("y", "/repo/y");
            y.source_profile = a.profile().to_string();
            a.update(|instances, _| {
                instances.push(y.clone());
                Ok(())
            })?;
            push_copy(b, x)?;
            push_copy(b, &y)?;
            let now = now_ms();
            let mut j1 = fresh_journal_entry(a, b, &y.id);
            j1.created_at_epoch_ms = now - 120_000;
            let mut j2 = fresh_journal_entry(b, a, &x.id);
            j2.ids.push(y.id.clone());
            j2.ids.sort();
            j2.created_at_epoch_ms = now - 60_000;
            let mut j3 = fresh_journal_entry(a, b, &x.id);
            j3.target_profile = "missing".to_string();
            j3.target_sessions_path = a.sessions_path().with_file_name("missing.json");
            j3.created_at_epoch_ms = now;
            record(&j1, a)?;
            record(&j2, b)?;
            record(&j3, a)
        };
        // (tag, arrange, reconcile source only, reports, rows (a, b), journals left in a,
        // entry permanently blacklisted)
        let cases: [(&str, Arrange, bool, usize, (usize, usize), usize, bool); 12] = [
            ("legacy", legacy, false, 1, (1, 1), 0, false),
            ("wrong-version", wrong_version, false, 1, (1, 1), 1, false),
            ("expired", expired, false, 1, (1, 1), 1, false),
            ("edited-after", edited_after, false, 1, (1, 1), 1, true),
            ("invalid-id", invalid_id, false, 1, (1, 1), 1, true),
            ("duplicate-ids", duplicate_ids, false, 0, (1, 0), 1, true),
            ("aliased", aliased, true, 0, (1, 0), 1, true),
            (
                "target-duplicate",
                target_duplicate,
                false,
                1,
                (1, 2),
                1,
                false,
            ),
            ("same-profile", same_profile, true, 1, (2, 0), 0, false),
            (
                "newer-unresolved",
                newer_unresolved,
                false,
                1,
                (1, 1),
                1,
                false,
            ),
            ("newer-opaque", newer_opaque, false, 1, (1, 1), 1, false),
            ("shadowed-batch", shadowed_batch, false, 2, (2, 2), 2, false),
        ];
        let mut failures = Vec::new();
        for (tag, arrange, source_only, reports, rows, journals, blacklisted) in cases {
            let check = || -> Result<()> {
                let (_temp, _guard, a, b, before, _after) = setup_recovery_env(tag)?;
                arrange(&a, &b, &before)?;
                let outcome = if source_only {
                    reconcile_loaded(&[&a])
                } else {
                    reconcile_loaded(&[&a, &b])
                };
                anyhow::ensure!(!outcome.repaired, "arbitrated");
                anyhow::ensure!(outcome.reports.len() == reports, "{:?}", outcome.reports);
                anyhow::ensure!((a.load()?.len(), b.load()?.len()) == rows, "rows changed");
                anyhow::ensure!(journal_entry_count(&a) == journals, "journal consumed");
                if blacklisted {
                    anyhow::ensure!(
                        unusable_journal_entries_contains(&first_journal_path(&a)),
                        "entry not blacklisted"
                    );
                }
                if let Some(report) = outcome.reports.first().filter(|_| tag == "legacy") {
                    let message = report.actionable_message();
                    anyhow::ensure!(report.id == before.id && report.copies.len() == 2);
                    anyhow::ensure!(
                        message.contains(&before.id)
                            && message.contains("sessions.json")
                            && message.contains(a.profile())
                            && message.contains(b.profile()),
                        "{message}"
                    );
                }
                Ok(())
            };
            if let Err(error) = check() {
                failures.push(format!("{tag}: {error:#}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
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
    fn profile_move_waits_for_original_transition_with_external_aliases() -> Result<()> {
        for metadata_abort in [false, true] {
            let temp = tempfile::tempdir()?;
            let _guard = isolate_app_dir_at(&temp.path().join("home"));
            let app = super::super::get_app_dir()?;
            fs::create_dir_all(app.join("profiles"))?;
            let mut stores = Vec::new();
            for name in ["source", "target"] {
                let physical = temp.path().join(name);
                let logical = app.join("profiles").join(name);
                fs::create_dir(&physical)?;
                std::os::unix::fs::symlink(&physical, &logical)?;
                stores.push(Storage::new_for_test_path(
                    name,
                    logical.join("sessions.json"),
                ));
            }
            let target = stores.pop().unwrap();
            let source = stores.pop().unwrap();
            let mut before = Instance::new("transition", "/repo/transition");
            before.source_profile = "source".into();
            before.group_path = "work".into();
            if metadata_abort {
                before.runner_journal =
                    super::super::runner_journal::RunnerExecutionJournal::legacy_unknown();
                super::super::retained_intents::initialize_legacy_in(&app)?;
            }
            source.update(|rows, groups| {
                rows.push(before.clone());
                groups.push(Group::new("work", "work"));
                Ok(())
            })?;
            target.update(|_, _| Ok(()))?;
            let prepared = if metadata_abort {
                Some(super::super::retained_intents::capture(
                    &source, &before.id,
                )?)
            } else {
                None
            };
            let mut after = before.clone();
            after.group_path = "moved".into();
            let source_bytes = fs::read(source.sessions_path())?;
            let target_bytes = fs::read(target.sessions_path())?;
            let held =
                acquire_storage_flock(&app, crate::migrations::v027_isolate_sandbox_stores::LOCK)?;
            let (contended_tx, contended_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let moving_source = source.clone();
            let moving_target = target.clone();
            let expected = before.id.clone();
            let worker = std::thread::spawn(move || {
                let _observer = observe_lock_contention_for_test(contended_tx);
                let result = match prepared {
                    Some(prepared) => super::super::retained_intents::abort(&prepared).map(|_| ()),
                    None => moving_source
                        .move_instances_to(
                            &moving_target,
                            &[(before, after)],
                            &GroupMovePlan::single("work", "moved"),
                            |_, _| Ok(()),
                        )
                        .map(|_| ()),
                };
                done_tx.send(result).unwrap();
            });
            let contended = contended_rx.recv_timeout(Duration::from_secs(2));
            let unchanged = fs::read(source.sessions_path())? == source_bytes
                && fs::read(target.sessions_path())? == target_bytes;
            drop(held);
            done_rx.recv_timeout(Duration::from_secs(2))??;
            worker.join().unwrap();
            assert_eq!(
                contended
                    .expect("consumer did not wait for its original application transition fence"),
                app.join(crate::migrations::v027_isolate_sandbox_stores::LOCK)
            );
            assert!(
                unchanged,
                "consumer published through the original transition fence"
            );
            assert!(source.load()?.is_empty());
            if metadata_abort {
                assert_eq!(fs::read(target.sessions_path())?, target_bytes);
                let original: Vec<Box<serde_json::value::RawValue>> =
                    serde_json::from_slice(&source_bytes)?;
                let retained = super::super::retained_intents::retained_raw_owners_in(&app)?;
                assert_eq!(retained[0].get(), original[0].get());
            } else {
                let target = target.load()?;
                assert_eq!(target[0].id, expected);
                assert_eq!(target[0].group_path, "moved");
            }
        }
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
    fn recovery_backups_keep_the_newest_three_and_sync_their_directory() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("profile/sessions.json");
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, b"[]")?;
        for stamp in 1..=5 {
            fs::write(
                path.with_file_name(format!("sessions.json.pre-recovery-{stamp}")),
                stamp.to_string(),
            )?;
        }
        let stamps = || -> Result<Vec<u128>> {
            let mut stamps: Vec<u128> = fs::read_dir(path.parent().unwrap())?
                .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .filter_map(|name| {
                    name.to_string_lossy()
                        .strip_prefix("sessions.json.pre-recovery-")
                        .and_then(|value| value.parse().ok())
                })
                .collect();
            stamps.sort();
            Ok(stamps)
        };
        let mut synced = Vec::new();
        let backups = repair_backups(&path)?;

        prune_backups_beside(&path, &backups, |candidate| {
            synced.push(candidate.to_path_buf());
            Ok(())
        })?;
        assert_eq!(
            synced,
            vec![path.clone()],
            "prune syncs the backup directory"
        );
        assert_eq!(
            stamps()?,
            [4, 5],
            "the prune leaves room for the caller's own copy"
        );

        backup_before_repair(&path)?;
        let stamps = stamps()?;
        assert_eq!(stamps.len(), BACKUPS_TO_KEEP);
        assert_eq!(&stamps[..2], &[4, 5], "oldest backups are pruned");
        Ok(())
    }

    /// Below the saturated end of the stamp space, a backup is named strictly
    /// above every sibling, so a second one cannot land on the first. The
    /// planted sibling carries a far-future stamp, so this fails against a
    /// `SystemTime::now()` naming rule whatever the clock and the filesystem do.
    /// The saturated case sorts the other way, and its test says so.
    #[test]
    fn a_backup_is_named_above_a_sibling_below_the_saturated_space() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        let planted = u128::MAX / 2;
        fs::write(
            path.with_file_name(format!("sessions.json.pre-migration-{planted}")),
            b"planted",
        )?;
        fs::write(&path, b"live")?;

        backup_before_migration(&path)?;

        let backups = migration_backups(&path)?;
        assert_eq!(backups.len(), 2, "the planted sibling must survive");
        assert_eq!(backups[0].0, planted);
        assert!(
            backups[1].0 > planted,
            "new copy must sort last: {backups:?}"
        );
        assert_eq!(fs::read(&backups[1].1)?, b"live");
        fs::write(&path, b"later")?;
        backup_before_migration(&path)?;
        let backups = migration_backups(&path)?;
        assert_eq!(backups.len(), 3, "an earlier copy must never be reused");
        assert_eq!(fs::read(&backups[1].1)?, b"live");
        assert_eq!(fs::read(&backups[2].1)?, b"later");
        Ok(())
    }

    /// An entry the prune cannot remove must not stop it. The fixture puts a
    /// directory at the oldest name and a removable file right after it, so a
    /// prune that broke on the failure would leave the second one behind: the
    /// continuation is what the assertions below distinguish.
    #[test]
    fn a_prune_that_cannot_remove_the_oldest_still_removes_the_next() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        let blocked = path.with_file_name("sessions.json.pre-migration-1");
        let removable = path.with_file_name("sessions.json.pre-migration-2");
        fs::create_dir(&blocked)?;
        fs::write(&removable, b"two")?;
        fs::write(
            path.with_file_name("sessions.json.pre-migration-3"),
            b"three",
        )?;
        fs::write(
            path.with_file_name(format!("sessions.json.pre-migration-{}", u128::MAX)),
            b"top",
        )?;
        fs::write(&path, b"live")?;

        backup_before_migration(&path)?;

        assert!(blocked.is_dir(), "the entry it could not remove stays");
        assert!(!removable.exists(), "the prune carried on past the failure");
        assert!(
            migration_backups(&path)?
                .iter()
                .any(|(_, candidate)| fs::read(candidate).is_ok_and(|bytes| bytes == b"live")),
            "the copy this call made must survive"
        );
        Ok(())
    }

    /// Foreign entries past any clock this build can read fill the retention
    /// window. The copy this call makes must survive the prune it triggers, so
    /// the test plants enough of them that the prune has work to do.
    #[test]
    fn a_saturated_stamp_space_keeps_the_copy_its_own_prune_would_take() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        for planted in [
            1_000_000_000_000_000_000_000_000_000_000u128,
            2_000_000_000_000_000_000_000_000_000_000u128,
            u128::MAX,
        ] {
            fs::write(
                path.with_file_name(format!("sessions.json.pre-migration-{planted}")),
                format!("planted-{planted}"),
            )?;
        }
        fs::write(&path, b"first")?;

        backup_before_migration(&path)?;

        let backups = migration_backups(&path)?;
        assert_eq!(
            backups.len(),
            BACKUPS_TO_KEEP,
            "the window stays bounded: {backups:?}"
        );
        assert_eq!(
            fs::read(&backups[0].1)?,
            b"first",
            "the copy this call made must outlive its own prune"
        );
        Ok(())
    }

    /// Two copies in a row against one saturated sibling: each call must keep
    /// its own copy and neither may reuse a name already on disk.
    #[test]
    fn a_saturated_stamp_space_never_reuses_a_taken_name() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        fs::write(
            path.with_file_name(format!("sessions.json.pre-migration-{}", u128::MAX)),
            b"planted",
        )?;

        fs::write(&path, b"first")?;
        backup_before_migration(&path)?;
        fs::write(&path, b"second")?;
        backup_before_migration(&path)?;

        let backups = migration_backups(&path)?;
        assert_eq!(backups.len(), 3, "both copies must land: {backups:?}");
        let landed: Vec<Vec<u8>> = backups
            .iter()
            .map(|(_, candidate)| fs::read(candidate).unwrap())
            .collect();
        assert!(landed.contains(&b"planted".to_vec()));
        assert!(landed.contains(&b"first".to_vec()));
        assert!(landed.contains(&b"second".to_vec()));
        Ok(())
    }

    /// A downgrade is served from the migration ring, and a repair's own copies
    /// are interchangeable while these are not, so the two must not share a
    /// budget: two repairs used to evict the one copy an older build can read.
    #[test]
    fn repairs_never_evict_the_migration_ring() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        for content in [b"pre-v029".as_slice(), b"post-v029"] {
            fs::write(&path, content)?;
            backup_before_migration(&path)?;
        }
        let downgrade_copy = fs::read(&migration_backups(&path)?[0].1)?;

        for _ in 0..4 {
            backup_before_repair(&path)?;
        }

        assert_eq!(
            migration_backups(&path)?.len(),
            2,
            "the migration ring must be untouched by repairs"
        );
        assert_eq!(
            fs::read(&migration_backups(&path)?[0].1)?,
            downgrade_copy,
            "the copy an older build can read must still be the oldest"
        );
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
    /// The creating constructor is called with the session identity flock held.
    /// Resolving an empty profile the unlocked way can reach the first-profile
    /// bootstrap, which takes that same flock on a fresh descriptor and waits on
    /// the caller's own lock, so the call has to come back rather than hang.
    #[test]
    fn open_or_create_under_the_identity_lock_resolves_an_empty_profile() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watcher = FileWatchService::noop();
        std::thread::spawn(move || {
            let _identity_lock =
                crate::session::acquire_session_identity_lock().expect("identity lock");
            let _ = done_tx.send(Storage::open_or_create("", watcher).map(|s| s.profile));
        });

        let profile = done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("open_or_create blocked on the identity lock it already holds")
            .expect("the first profile is created, not an error");
        assert_eq!(profile, "main");
    }
}

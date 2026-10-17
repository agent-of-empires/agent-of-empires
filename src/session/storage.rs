//! Session storage with a stable app-root fence before identity, title, lifecycle and storage locks.
//! Ownership covers authoritative reload, admission and publication.
//! Git subprocesses, hooks and container waits run unlocked under durable reservations.
//! Multi-store operations order save mutexes by identity and flocks by physical file identity.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;

use super::environment::shell_escape_script_word;
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
#[derive(Debug)]
struct HeldFlock {
    file: fs::File,
    path: PathBuf,
}

impl Drop for HeldFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug)]
pub(crate) struct StorageFlock {
    held: HeldFlock,
    ownership: Option<OwnershipGuard>,
}

/// Shared leases protect reads; exclusive leases fence short namespace transactions.
#[derive(Debug)]
pub(crate) struct OwnershipGuard {
    _held: HeldFlock,
    exclusive: bool,
    nonblocking: bool,
}

impl OwnershipGuard {
    pub(crate) fn is_nonblocking(&self) -> bool {
        self.nonblocking
    }

    pub(crate) fn app_dir(&self) -> &Path {
        self._held
            .path
            .parent()
            .expect("ownership lock has an application directory")
    }

    fn verify(&self) -> Result<()> {
        anyhow::ensure!(
            fs::canonicalize(self.app_dir())? == fs::canonicalize(super::get_app_dir_path()?)?,
            "ownership guard belongs to a different application directory"
        );
        Ok(())
    }

    pub(crate) fn require_exclusive(&self) -> Result<()> {
        self.verify()?;
        anyhow::ensure!(
            self.exclusive,
            "checkout claims require exclusive ownership"
        );
        Ok(())
    }
}

fn ownership_lock(shared: bool) -> Result<OwnershipGuard> {
    let app_dir = get_app_dir()?;
    let lock = if shared {
        acquire_storage_shared_flock(&app_dir, ".workspace-claim.lock")?
    } else {
        acquire_storage_flock(&app_dir, ".workspace-claim.lock")?
    };
    Ok(OwnershipGuard {
        _held: lock.held,
        exclusive: !shared,
        nonblocking: false,
    })
}

/// Never reacquire ownership for the same app directory while holding it.
/// Flocks belong to open file descriptions; use the `*_with_ownership` APIs.
pub(crate) fn acquire_ownership_read() -> Result<OwnershipGuard> {
    ownership_lock(true)
}
pub(crate) fn try_acquire_ownership_read() -> Result<Option<OwnershipGuard>> {
    let app_dir = get_app_dir()?;
    let (file, path) = open_storage_lock_file(&app_dir, ".workspace-claim.lock")?;
    match FileExt::try_lock_shared(&file) {
        Ok(()) => Ok(Some(OwnershipGuard {
            _held: HeldFlock { file, path },
            exclusive: false,
            nonblocking: true,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn acquire_ownership_lock() -> Result<OwnershipGuard> {
    ownership_lock(false)
}

pub(crate) fn try_acquire_ownership_lock() -> Result<Option<OwnershipGuard>> {
    Ok(
        try_acquire_storage_flock(&get_app_dir()?, ".workspace-claim.lock")?.map(|lock| {
            OwnershipGuard {
                _held: lock.held,
                exclusive: true,
                nonblocking: true,
            }
        }),
    )
}

fn app_dir_for_profile_dir(profile_dir: &Path) -> &Path {
    profile_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
        .and_then(Path::parent)
        .unwrap_or(profile_dir)
}

fn acquire_transition_flocks_for_profile_dirs(
    profile_dirs: &[&Path],
    nonblocking: bool,
) -> Result<Vec<StorageFlock>> {
    let mut app_dirs: Vec<PathBuf> = profile_dirs
        .iter()
        .map(|dir| app_dir_for_profile_dir(dir).to_path_buf())
        .collect();
    app_dirs.sort();
    app_dirs.dedup();
    app_dirs
        .iter()
        .map(|dir| {
            let (file, path) = open_existing_storage_lock_file(
                dir,
                crate::migrations::v027_isolate_sandbox_stores::LOCK,
            )?;
            if nonblocking {
                FileExt::try_lock_shared(&file).context("Session operation in progress")?;
                Ok(StorageFlock {
                    held: HeldFlock { file, path },
                    ownership: None,
                })
            } else {
                acquire_open_storage_shared_flock(file, &path)
            }
        })
        .collect()
}

pub(crate) fn filesystem_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(unix)]
fn same_filesystem_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    filesystem_identity(left) == filesystem_identity(right)
}

#[cfg(not(unix))]
fn same_filesystem_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    // Portable metadata exposes no stable file identity.
    false
}

struct OpenStorageLock {
    file: fs::File,
    path: PathBuf,
    key: (u64, u64),
}

impl OpenStorageLock {
    fn new(file: fs::File, path: PathBuf) -> Result<Self> {
        Ok(Self {
            key: filesystem_identity(&file.metadata()?),
            file,
            path,
        })
    }

    fn acquire(self, nonblocking: bool) -> Result<StorageFlock> {
        if nonblocking {
            self.file
                .try_lock_exclusive()
                .context("Session operation in progress")?;
            Ok(StorageFlock {
                held: HeldFlock {
                    file: self.file,
                    path: self.path,
                },
                ownership: None,
            })
        } else {
            acquire_open_storage_flock(self.file, &self.path)
        }
    }
}

fn acquire_open_storage_flocks(
    mut files: Vec<OpenStorageLock>,
    nonblocking: bool,
) -> Result<Vec<StorageFlock>> {
    files.sort_unstable_by_key(|file| file.key);
    files.dedup_by_key(|file| file.key);
    files
        .into_iter()
        .map(|file| file.acquire(nonblocking))
        .collect()
}

fn acquire_two_open_storage_flocks(
    first: OpenStorageLock,
    second: OpenStorageLock,
    nonblocking: bool,
) -> Result<(StorageFlock, StorageFlock)> {
    let (first, second) = if first.key < second.key {
        (first, second)
    } else {
        (second, first)
    };
    Ok((first.acquire(nonblocking)?, second.acquire(nonblocking)?))
}

fn lock_two_save_mutexes<'a>(
    source: &'a Storage,
    target: &'a Storage,
    nonblocking: bool,
) -> Result<(
    std::sync::MutexGuard<'a, ()>,
    Option<std::sync::MutexGuard<'a, ()>>,
)> {
    let (first, second) = if Arc::as_ptr(&source.save_lock) < Arc::as_ptr(&target.save_lock) {
        (&source.save_lock, &target.save_lock)
    } else {
        (&target.save_lock, &source.save_lock)
    };
    let first_guard = lock_save_mutex(first, nonblocking)?;
    let second_guard = (!Arc::ptr_eq(first, second))
        .then(|| lock_save_mutex(second, nonblocking))
        .transpose()?;
    Ok((first_guard, second_guard))
}

fn lock_save_mutex(lock: &Mutex<()>, nonblocking: bool) -> Result<std::sync::MutexGuard<'_, ()>> {
    if nonblocking {
        match lock.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into())
            }
        }
    } else {
        Ok(lock.lock().unwrap_or_else(|error| error.into_inner()))
    }
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
    open_existing_storage_lock_file(dir, name)
}

fn open_existing_storage_lock_file(dir: &Path, name: &str) -> Result<(fs::File, PathBuf)> {
    anyhow::ensure!(
        fs::metadata(dir)?.is_dir(),
        "storage directory is unavailable"
    );
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
static ROOT_CONTENTION_OBSERVER: parking_lot::Mutex<
    Option<(PathBuf, std::sync::mpsc::Sender<PathBuf>)>,
> = parking_lot::Mutex::new(None);

#[cfg(test)]
pub(crate) fn observe_root_contention_for_test(
    sender: std::sync::mpsc::Sender<PathBuf>,
) -> impl Drop {
    struct Observer;
    impl Drop for Observer {
        fn drop(&mut self) {
            ROOT_CONTENTION_OBSERVER.lock().take();
        }
    }
    let path = get_app_dir().unwrap().join(".workspace-claim.lock");
    assert!(ROOT_CONTENTION_OBSERVER
        .lock()
        .replace((path, sender))
        .is_none());
    Observer
}

#[cfg(test)]
type UpdateObserver = Box<dyn FnMut(&Storage)>;

#[cfg(test)]
thread_local! {
    static UPDATE_OBSERVER: std::cell::RefCell<Option<UpdateObserver>> =
        const { std::cell::RefCell::new(None) };
}

/// Call `observer` at the start of every `Storage::update` on this thread until the guard drops.
#[cfg(test)]
pub(crate) fn observe_updates_for_test(observer: impl FnMut(&Storage) + 'static) -> impl Drop {
    struct Observer(std::marker::PhantomData<std::rc::Rc<()>>);
    impl Drop for Observer {
        fn drop(&mut self) {
            UPDATE_OBSERVER.with(|slot| slot.borrow_mut().take());
        }
    }
    UPDATE_OBSERVER.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(observer)).is_none());
    });
    Observer(std::marker::PhantomData)
}

#[cfg(test)]
fn report_update_for_test(storage: &Storage) {
    // Taken out while it runs, so an update inside the observer does not re-enter it.
    let Some(mut observer) = UPDATE_OBSERVER.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    observer(storage);
    UPDATE_OBSERVER.with(|slot| {
        slot.borrow_mut().get_or_insert(observer);
    });
}

#[cfg(test)]
fn report_lock_contention_for_test(path: &Path) {
    LOCK_CONTENTION_OBSERVER.with(|slot| {
        if let Some(sender) = slot.borrow_mut().take() {
            let _ = sender.send(path.to_path_buf());
        }
    });
    let mut observer = ROOT_CONTENTION_OBSERVER.lock();
    if observer
        .as_ref()
        .is_some_and(|(expected, _)| expected == path)
    {
        let (_, sender) = observer.take().unwrap();
        let _ = sender.send(path.to_path_buf());
    }
}

fn acquire_open_storage_flock(file: fs::File, path: &Path) -> Result<StorageFlock> {
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
        held: HeldFlock {
            file,
            path: path.to_path_buf(),
        },
        ownership: None,
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
        held: HeldFlock {
            file,
            path: path.to_path_buf(),
        },
        ownership: None,
    })
}

/// Acquire the app-wide session identity-mutation lock.
pub(crate) fn acquire_session_identity_lock() -> Result<StorageFlock> {
    let ownership = acquire_ownership_read()?;
    let mut lock = acquire_session_identity_lock_with_ownership(&ownership)?;
    lock.ownership = Some(ownership);
    Ok(lock)
}

pub(crate) fn acquire_session_identity_lock_with_ownership(
    ownership: &OwnershipGuard,
) -> Result<StorageFlock> {
    ownership.verify()?;
    acquire_storage_flock(ownership.app_dir(), SESSION_IDENTITY_LOCK_FILENAME)
}

pub(crate) fn try_acquire_session_identity_lock_with_ownership(
    ownership: &OwnershipGuard,
) -> Result<Option<StorageFlock>> {
    ownership.verify()?;
    try_acquire_storage_flock(ownership.app_dir(), SESSION_IDENTITY_LOCK_FILENAME)
}

/// Serialize one session's title commit and post-commit tmux rekey across profiles and
/// processes.
pub(crate) fn acquire_session_title_lock(instance_id: &str) -> Result<StorageFlock> {
    let ownership = acquire_ownership_read()?;
    let mut lock = acquire_session_title_lock_with_ownership(&ownership, instance_id)?;
    lock.ownership = Some(ownership);
    Ok(lock)
}

pub(crate) fn acquire_session_title_lock_with_ownership(
    ownership: &OwnershipGuard,
    instance_id: &str,
) -> Result<StorageFlock> {
    ownership.verify()?;
    super::validate_instance_id(instance_id)
        .context("refusing session title lock for invalid instance id")?;
    acquire_storage_flock(
        ownership.app_dir(),
        &format!("{SESSION_TITLE_LOCK_PREFIX}{instance_id}.lock"),
    )
}

pub(crate) fn try_acquire_session_title_lock_with_ownership(
    ownership: &OwnershipGuard,
    instance_id: &str,
) -> Result<Option<StorageFlock>> {
    ownership.verify()?;
    super::validate_instance_id(instance_id)?;
    try_acquire_storage_flock(
        ownership.app_dir(),
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
    let (file, path) = open_storage_lock_file(dir, name)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(StorageFlock {
            held: HeldFlock { file, path },
            ownership: None,
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

fn record_move_journal(
    ownership: &OwnershipGuard,
    source: &Storage,
    target: &Storage,
    ids: &std::collections::HashSet<&str>,
    source_instances: &[Instance],
    moved: &[Instance],
    plan: &MoveTransactionPlan<'_>,
) -> Result<super::move_journal::JournalTransaction> {
    let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
    ids.sort();
    let entry = super::move_journal::MoveJournalEntry {
        version: super::move_journal::MOVE_JOURNAL_VERSION,
        ids,
        source_profile: source.profile.clone(),
        target_profile: target.profile.clone(),
        source_sessions_path: source.sessions_path.clone(),
        target_sessions_path: target.sessions_path.clone(),
        group_move_source_path: plan.group_move.source_path.clone(),
        group_move_target_path: plan.group_move.target_path.clone(),
        group_move_subtree: plan.group_move.move_subtree,
        created_at_epoch_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
        authority: super::move_journal::MoveAuthority::Metadata {
            source: super::move_journal::ProfileAuthority::capture(source)?,
            target: super::move_journal::ProfileAuthority::capture(target)?,
            rows: moved
                .iter()
                .map(|after| -> Result<_> {
                    let before = source_instances
                        .iter()
                        .find(|row| row.id == after.id)
                        .context("move journal source incarnation disappeared")?;
                    Ok(super::move_journal::RowDelta {
                        before: before.clone(),
                        after: after.clone(),
                    })
                })
                .collect::<Result<_>>()?,
        },
    };
    super::move_journal::record_live(ownership, &entry)
        .context("recording the durable move journal failed; no move effect or profile row changed")
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
    } else if super::groups::group_is_unused(
        &plan.source_path,
        source_instances
            .iter()
            .map(|instance| instance.group_path.as_str()),
        source_groups.iter().map(|group| group.path.as_str()),
    ) {
        source_groups.retain(|group| group.path != plan.source_path);
    }

    // Re-tree both sides so a group implied only by a moved instance's path materialises as
    // an explicit row.
    *source_groups =
        super::GroupTree::new_with_groups(source_instances, source_groups).get_all_groups();
    *target_groups =
        super::GroupTree::new_with_groups(target_instances, target_groups).get_all_groups();
}

impl Storage {
    fn acquire_ownership_bound(&self, shared: bool) -> Result<OwnershipGuard> {
        let profile_dir = self
            .sessions_path
            .parent()
            .context("sessions path has no parent")?;
        let (file, path) = open_existing_storage_lock_file(
            app_dir_for_profile_dir(profile_dir),
            ".workspace-claim.lock",
        )?;
        let lock = if shared {
            acquire_open_storage_shared_flock(file, &path)?
        } else {
            acquire_open_storage_flock(file, &path)?
        };
        Ok(OwnershipGuard {
            _held: lock.held,
            exclusive: !shared,
            nonblocking: false,
        })
    }

    pub(crate) fn acquire_ownership_lock_bound(&self) -> Result<OwnershipGuard> {
        self.acquire_ownership_bound(false)
    }

    pub(crate) fn acquire_ownership_read_bound(&self) -> Result<OwnershipGuard> {
        self.acquire_ownership_bound(true)
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
    pub(crate) fn new_for_test_profile(profile: &str) -> Self {
        let directory = super::get_app_dir_path()
            .expect("resolve fixture application namespace")
            .join("profiles")
            .join(profile);
        Self::new_for_test_path(profile, directory.join("sessions.json"))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_path(profile: &str, sessions_path: PathBuf) -> Self {
        let profile_dir = sessions_path.parent().expect("fixture profile directory");
        fs::create_dir_all(profile_dir).expect("create fixture profile directory");
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
        let profile = resolve_existing_profile(profile)?;
        let ownership = acquire_ownership_read()?;
        Self::open_with_ownership(&profile, file_watch, &ownership)
    }

    pub(crate) fn open_unwatched_with_ownership(
        profile: &str,
        ownership: &OwnershipGuard,
    ) -> Result<Self> {
        Self::open_with_ownership(profile, FileWatchService::noop(), ownership)
    }

    pub(crate) fn open_with_ownership(
        profile: &str,
        file_watch: Arc<FileWatchService>,
        ownership: &OwnershipGuard,
    ) -> Result<Self> {
        ownership.verify()?;
        anyhow::ensure!(
            !profile.is_empty(),
            "ownership-scoped storage needs an explicit profile"
        );
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

    /// Run explicitly before resolving command or daemon session snapshots.
    pub fn recover_pending_moves(file_watch: Arc<FileWatchService>) -> Result<Vec<String>> {
        use super::move_journal::MoveAuthority;
        let pending = super::move_journal::scan();
        let mut notices = Vec::new();
        for (path, reason) in pending.unreadable_dirs {
            notices.push(format!(
                "Move intent directory {} is unreadable: {reason}; inspect with `aoe intents list` before resolving its entries",
                path.display()
            ));
        }
        for (path, parsed) in pending.entries {
            let result = (|| -> Result<bool> {
                parsed.map_err(anyhow::Error::msg)?;
                let Some((journal, entry)) = super::move_journal::try_recovery_lease(&path)? else {
                    return Ok(true);
                };
                match &entry.authority {
                    MoveAuthority::Resource { .. } => {
                        anyhow::ensure!(
                            !entry.source_profile.is_empty(),
                            "recovery requires an explicit existing profile"
                        );
                        let source = Self::open(&entry.source_profile, Arc::clone(&file_watch));
                        recover_resource_journal(journal, entry, source)
                    }
                    MoveAuthority::Metadata { .. } => {
                        anyhow::ensure!(
                            !entry.source_profile.is_empty() && !entry.target_profile.is_empty(),
                            "recovery requires explicit existing profiles"
                        );
                        let source = Self::open(&entry.source_profile, Arc::clone(&file_watch))?;
                        let target = Self::open(&entry.target_profile, Arc::clone(&file_watch))?;
                        repair_metadata_entry_with_transaction(
                            &entry,
                            &[(source.profile(), &source), (target.profile(), &target)],
                            &journal,
                            sync_parent_directory,
                        )
                    }
                    MoveAuthority::Cleanup { .. } => recover_cleanup_journal(&journal, entry),
                    MoveAuthority::LegacyConflict { reason, .. } => anyhow::bail!("{reason}"),
                }
            })();
            match result {
                Ok(true) => {}
                Ok(false) => notices.push(format!(
                    "Move intent conflict at {} requires resolution; inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`",
                    path.display(), shell_escape_script_word(&path.to_string_lossy())
                )),
                Err(error) => notices.push(format!(
                    "Move intent conflict at {}: {error:#}; inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`",
                    path.display(), shell_escape_script_word(&path.to_string_lossy())
                )),
            }
        }
        Ok(notices)
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
        let ownership = self.acquire_ownership_read_bound()?;
        let mut lock =
            self.acquire_instance_lifecycle_lock_with_ownership(&ownership, instance_id)?;
        lock.ownership = Some(ownership);
        Ok(lock)
    }

    pub(crate) fn acquire_instance_lifecycle_lock_with_ownership(
        &self,
        ownership: &OwnershipGuard,
        instance_id: &str,
    ) -> Result<StorageFlock> {
        ownership.verify()?;
        self.verify_profile_available()?;
        super::validate_instance_id(instance_id)
            .context("refusing lifecycle lock for invalid instance id")?;
        let profile_dir = self
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no profile directory"))?;
        let (file, path) = open_existing_storage_lock_file(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )?;
        let lock = acquire_open_storage_flock(file, &path)?;
        self.verify_profile_available()?;
        Ok(lock)
    }

    pub(crate) fn try_acquire_instance_lifecycle_lock_with_ownership(
        &self,
        ownership: &OwnershipGuard,
        instance_id: &str,
    ) -> Result<Option<StorageFlock>> {
        ownership.verify()?;
        self.verify_profile_available()?;
        super::validate_instance_id(instance_id)?;
        let profile_dir = self
            .sessions_path
            .parent()
            .context("sessions path has no profile directory")?;
        let (file, path) = open_existing_storage_lock_file(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                self.verify_profile_available()?;
                Ok(Some(StorageFlock {
                    held: HeldFlock { file, path },
                    ownership: None,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn verify_profile_available(&self) -> Result<()> {
        let dir = self
            .sessions_path
            .parent()
            .context("sessions path has no profile directory")?;
        anyhow::ensure!(
            fs::metadata(dir)
                .with_context(|| format!("profile is no longer available: {}", dir.display()))?
                .is_dir(),
            "profile path is not a directory"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_save_lock_for_test<R>(&self, f: impl FnOnce() -> R) -> R {
        let _held = self.save_lock.lock().unwrap();
        f()
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

    pub(crate) fn file_watch(&self) -> Arc<FileWatchService> {
        Arc::clone(&self.file_watch)
    }

    /// Absolute path of this profile's `sessions.json`. Recovery and the
    /// duplicate-detection surface report it so users can act on exact files.
    pub(crate) fn sessions_path(&self) -> &Path {
        &self.sessions_path
    }

    /// Ownership decisions cannot quarantine or silently omit an undecodable owner.
    /// Callers serialize this read with the ownership and storage fences.
    pub(crate) fn load_strict_for_worktree_ownership(&self) -> Result<Vec<Instance>> {
        self.verify_profile_available()?;
        match fs::symlink_metadata(&self.sessions_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let content = fs::read_to_string(&self.sessions_path)?;
        let mut instances: Vec<Instance> = serde_json::from_str(&content).with_context(|| {
            format!("reading ownership rows in {}", self.sessions_path.display())
        })?;
        let mut ids = std::collections::HashSet::with_capacity(instances.len());
        for instance in &mut instances {
            instance.source_profile = self.profile.clone();
            instance.set_file_watch(self.file_watch.clone());
            anyhow::ensure!(
                !instance.id.is_empty() && ids.insert(instance.id.as_str()),
                "ambiguous session id in ownership inventory"
            );
        }
        self.verify_profile_available()?;
        Ok(instances)
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
                    // `source_profile` is never persisted, so a loaded row
                    // would otherwise resolve its agent against the default
                    // profile rather than the store it came from.
                    inst.source_profile = self.profile.clone();
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
        let ownership = self.acquire_ownership_read_bound()?;
        self.update_with_ownership(&ownership, f)
    }

    pub(crate) fn update_with_ownership<F, R>(&self, ownership: &OwnershipGuard, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.update_scoped(ownership, None, None, f)
    }

    /// `None` means contention: the mutation closure has not run.
    pub(crate) fn try_update_with_ownership<F, R>(
        &self,
        ownership: &OwnershipGuard,
        f: F,
    ) -> Result<Option<R>>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.try_update_scoped(ownership, None, f)
    }

    fn try_update_scoped<F, R>(
        &self,
        ownership: &OwnershipGuard,
        intent: Option<&super::move_journal::JournalTransaction>,
        f: F,
    ) -> Result<Option<R>>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        ownership.verify()?;
        self.verify_profile_available()?;
        let _mutex = match self.save_lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let profile_dir = self
            .sessions_path
            .parent()
            .context("sessions path has no profile directory")?;
        let (file, path) = open_storage_lock_file(
            app_dir_for_profile_dir(profile_dir),
            crate::migrations::v027_isolate_sandbox_stores::LOCK,
        )?;
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let _transition = StorageFlock {
            held: HeldFlock { file, path },
            ownership: None,
        };
        let (file, path) = open_existing_storage_lock_file(profile_dir, STORAGE_LOCK_FILENAME)?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let _flock = StorageFlock {
            held: HeldFlock { file, path },
            ownership: None,
        };
        self.verify_profile_available()?;
        #[cfg(test)]
        report_update_for_test(self);
        self.update_under_lock(intent, None, f).map(Some)
    }

    pub(crate) fn update_with_intent<F, R>(
        &self,
        ownership: &OwnershipGuard,
        intent: &super::move_journal::JournalTransaction,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        self.update_scoped(ownership, Some(intent), None, f)
    }

    fn update_scoped<F, R>(
        &self,
        ownership: &OwnershipGuard,
        intent: Option<&super::move_journal::JournalTransaction>,
        attested: Option<&Instance>,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        if ownership.nonblocking {
            anyhow::ensure!(
                attested.is_none(),
                "attested recovery requires blocking ownership"
            );
            return self
                .try_update_scoped(ownership, intent, f)?
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
        }
        ownership.verify()?;
        self.verify_profile_available()?;
        #[cfg(test)]
        report_update_for_test(self);
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
        self.verify_profile_available()?;
        let (file, path) = open_existing_storage_lock_file(profile_dir, STORAGE_LOCK_FILENAME)?;
        let _flock = acquire_open_storage_flock(file, &path)?;
        self.verify_profile_available()?;
        self.update_under_lock(intent, attested, f)
    }

    /// Apply one storage mutation while the caller already owns this profile's
    /// in-process save lock and cross-process storage flock.
    fn update_under_lock<F, R>(
        &self,
        intent: Option<&super::move_journal::JournalTransaction>,
        attested: Option<&Instance>,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        let (mut instances, mut groups) = self.load_with_groups()?;
        let admission = match attested {
            Some(expected) => {
                super::move_journal::ResourceAdmission::capture_attested(self, &instances, expected)
            }
            None => super::move_journal::ResourceAdmission::capture(self, &instances, intent),
        };
        let groups_before = groups.clone();
        let result = f(&mut instances, &mut groups)?;
        admission.validate(&instances)?;

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

        if intent.is_some() || attested.is_some() {
            atomic_write_verified(&self.sessions_path, &instances_buf)?;
            sync_parent_directory(&self.sessions_path)?;
        } else {
            atomic_write(&self.sessions_path, &instances_buf)?;
        }
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(result)
    }

    pub(crate) fn release_attested_binding_with_ownership(
        &self,
        ownership: &OwnershipGuard,
        expected: &Instance,
    ) -> Result<()> {
        use super::move_journal::{binding_path, lease_state, LeaseState};
        ownership.require_exclusive()?;
        let _identity = acquire_session_identity_lock_with_ownership(ownership)?;
        let _lifecycle =
            self.acquire_instance_lifecycle_lock_with_ownership(ownership, &expected.id)?;
        self.update_scoped(ownership, None, Some(expected), |rows, _| {
            let current = rows.iter_mut().find(|row| row.id == expected.id)
                .context("orphan-binding session disappeared")?;
            anyhow::ensure!(
                current.created_at == expected.created_at
                    && current.lifecycle_generation == expected.lifecycle_generation
                    && current.lifecycle_reservation == expected.lifecycle_reservation
                    && super::move_journal::same_namespace(current, expected),
                "orphan-binding authority changed"
            );
            let reservation = current.lifecycle_reservation.as_ref().context("session has no lifecycle reservation")?;
            let binding = reservation.resource_intent.as_deref().context("session has no resource intent binding")?;
            let path = binding_path(binding)?;
            anyhow::ensure!(
                matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "referenced journal exists; use aoe intents clear {} --actors-stopped", shell_escape_script_word(&path.to_string_lossy())
            );
            anyhow::ensure!(lease_state(&path)? != LeaseState::Live, "intent producer is still live");
            anyhow::ensure!(reservation.generation == current.lifecycle_generation, "orphan-binding generation changed");
            let (operation, generation) = (reservation.op, reservation.generation);
            current.release_lifecycle_reservation_if_owned(operation, generation);
            Ok(())
        })
    }

    /// Publish one row through the dual-profile commit, optionally under its resource intent.
    pub(crate) fn move_instance_to_with_ownership<F>(
        &self,
        ownership: &OwnershipGuard,
        target: &Storage,
        change: (&Instance, &Instance),
        account_swap: bool,
        mut validate_target: F,
        journal: Option<&super::move_journal::JournalTransaction>,
    ) -> Result<Instance>
    where
        F: FnMut(&[Instance], &Instance) -> Result<()>,
    {
        let (before, after) = change;
        let changes = [(before.clone(), after.clone())];
        let group_move = GroupMovePlan::single(&before.group_path, &after.group_path);
        let mut moved = self.move_instances_to_inner_with_ownership(
            ownership,
            target,
            &changes,
            MoveTransactionPlan {
                group_move: &group_move,
                merge_complete_post: true,
                account_swap,
            },
            (
                |instances, candidates| validate_target(instances, &candidates[0]),
                |_| Ok(()),
                journal,
            ),
            sync_resolved_parent_directory,
        )?;
        Ok(moved.remove(0))
    }

    /// Move a batch between profiles as one dual-locked transaction.
    pub(crate) fn move_instances_to_with_ownership<F>(
        &self,
        ownership: &OwnershipGuard,
        target: &Storage,
        changes: &[(Instance, Instance)],
        group_move: &GroupMovePlan,
        validate_target: F,
    ) -> Result<Vec<Instance>>
    where
        F: FnMut(&[Instance], &[Instance]) -> Result<()>,
    {
        self.move_instances_to_inner_with_ownership(
            ownership,
            target,
            changes,
            MoveTransactionPlan {
                group_move,
                merge_complete_post: false,
                account_swap: false,
            },
            (validate_target, |_| Ok(()), None),
            sync_resolved_parent_directory,
        )
    }

    #[cfg(test)]
    fn move_instances_to_inner<F, B, S>(
        &self,
        target: &Storage,
        changes: &[(Instance, Instance)],
        plan: MoveTransactionPlan<'_>,
        validate_target: F,
        before_commit: B,
        sync_target_parent: S,
    ) -> Result<Vec<Instance>>
    where
        F: FnMut(&[Instance], &[Instance]) -> Result<()>,
        B: FnOnce(&[Instance]) -> Result<()>,
        S: FnMut(&Path) -> Result<()>,
    {
        let ownership = acquire_ownership_lock()?;
        self.move_instances_to_inner_with_ownership(
            &ownership,
            target,
            changes,
            plan,
            (validate_target, before_commit, None),
            sync_target_parent,
        )
    }

    fn move_instances_to_inner_with_ownership<F, B, S>(
        &self,
        ownership: &OwnershipGuard,
        target: &Storage,
        changes: &[(Instance, Instance)],
        plan: MoveTransactionPlan<'_>,
        callbacks: (F, B, Option<&super::move_journal::JournalTransaction>),
        mut sync_target_parent: S,
    ) -> Result<Vec<Instance>>
    where
        F: FnMut(&[Instance], &[Instance]) -> Result<()>,
        B: FnOnce(&[Instance]) -> Result<()>,
        S: FnMut(&Path) -> Result<()>,
    {
        ownership.require_exclusive()?;
        let (mut validate_target, before_commit, existing_journal) = callbacks;
        let existing_entry = existing_journal.map(|journal| journal.read()).transpose()?;
        let resource_owner = existing_entry
            .as_ref()
            .and_then(|entry| match &entry.authority {
                super::move_journal::MoveAuthority::Resource { intent } => Some(intent.as_ref()),
                _ => None,
            });
        let source_authority = resource_owner
            .map(|_| super::move_journal::ProfileAuthority::capture(self))
            .transpose()?;
        let mut before_commit = Some(before_commit);
        let mut prepared_journal = None;
        self.verify_profile_available()?;
        target.verify_profile_available()?;
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

        let _mutexes = lock_two_save_mutexes(self, target, ownership.nonblocking)?;
        let (first, second) = (self, target);
        let first_dir = first
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let second_dir = second
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let _transition_flocks = acquire_transition_flocks_for_profile_dirs(
            &[first_dir, second_dir],
            ownership.nonblocking,
        )?;
        let (first_lock_file, first_lock_path) =
            open_existing_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
        let (second_lock_file, second_lock_path) =
            open_existing_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
        if same_filesystem_identity(&first_lock_file.metadata()?, &second_lock_file.metadata()?) {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical storage lock"
            ));
        }
        let _flocks = acquire_two_open_storage_flocks(
            OpenStorageLock::new(first_lock_file, first_lock_path)?,
            OpenStorageLock::new(second_lock_file, second_lock_path)?,
            ownership.nonblocking,
        )?;
        self.verify_profile_available()?;
        target.verify_profile_available()?;

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
        let mut published = std::collections::HashSet::new();
        for (before, after) in changes {
            if !ids.insert(before.id.as_str()) {
                return Err(anyhow!("duplicate session id in profile move batch"));
            }
            let source = source_instances
                .iter()
                .find(|instance| instance.id == before.id)
                .ok_or_else(|| anyhow!("Session not found in source profile"))?;
            anyhow::ensure!(
                source.created_at == before.created_at,
                "Session incarnation changed before profile move"
            );
            let owned_resource = match (resource_owner, source_authority.as_ref(), existing_journal)
            {
                (Some(intent), Some(profile), Some(journal)) => {
                    intent.row_is_reserved(profile, source, journal.id())
                }
                _ => false,
            };
            if source.has_fresh_lifecycle_reservation(chrono::Utc::now()) {
                anyhow::ensure!(
                    owned_resource
                        || (before
                            .lifecycle_reservation
                            .as_ref()
                            .is_some_and(|lease| lease.resource_intent.is_none())
                            && source
                                .lifecycle_reservation
                                .as_ref()
                                .is_some_and(|lease| lease.resource_intent.is_none())
                            && before.lifecycle_reservation_is_owned(
                                super::LifecycleOperation::Rename,
                                before.lifecycle_generation
                            )
                            && source.lifecycle_reservation_is_owned(
                                super::LifecycleOperation::Rename,
                                before.lifecycle_generation
                            )),
                    "Cannot move a session while a lifecycle operation is in progress"
                );
            }
            let mut candidate = source.clone();
            if plan.merge_complete_post {
                candidate.merge_resource_diff(before, after, plan.account_swap)?;
            } else {
                candidate.merge_user_action_diff(before, after);
            }
            if owned_resource {
                candidate.lifecycle_reservation = None;
            } else if before.lifecycle_reservation_is_owned(
                super::LifecycleOperation::Rename,
                before.lifecycle_generation,
            ) {
                anyhow::ensure!(
                    source.lifecycle_reservation_is_owned(
                        super::LifecycleOperation::Rename,
                        before.lifecycle_generation
                    ),
                    "Profile move rename reservation was superseded"
                );
                candidate.release_lifecycle_reservation_if_owned(
                    super::LifecycleOperation::Rename,
                    before.lifecycle_generation,
                );
            }
            candidate.source_profile.clone_from(&target.profile);
            if let Some(existing) = target_instances.iter().find(|row| row.id == before.id) {
                anyhow::ensure!(
                    owned_resource
                        && existing.created_at == candidate.created_at
                        && existing.lifecycle_generation == candidate.lifecycle_generation
                        && existing.lifecycle_reservation.is_none()
                        && super::move_journal::same_namespace(existing, &candidate),
                    "Session already exists in target profile"
                );
                anyhow::ensure!(
                        super::move_journal::source_changes_preserved(after, &candidate, existing)?,
                        "Source annotations changed after target publication; both copies were preserved"
                    );
                published.insert(before.id.as_str());
                moved.push(existing.clone());
            } else {
                moved.push(candidate);
            }
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

        if existing_journal.is_none() && prepared_journal.is_none() {
            prepared_journal = Some(record_move_journal(
                ownership,
                self,
                target,
                &ids,
                &source_instances,
                &moved,
                &plan,
            )?);
        }
        let journal = existing_journal
            .or(prepared_journal.as_ref())
            .context("profile move has no durable transaction")?;
        let source_admission =
            super::move_journal::ResourceAdmission::capture(self, &source_instances, Some(journal));
        let target_admission = super::move_journal::ResourceAdmission::capture(
            target,
            &target_instances,
            Some(journal),
        );
        let source_groups_before = serde_json::to_vec_pretty(&source_groups)?;
        let target_instances_before = serde_json::to_vec_pretty(&target_instances)?;
        let target_groups_before = serde_json::to_vec_pretty(&target_groups)?;

        let source_instances_before = serde_json::to_vec_pretty(&source_instances)?;
        source_instances.retain(|instance| !ids.contains(instance.id.as_str()));
        target_instances.extend(
            moved
                .iter()
                .filter(|row| !published.contains(row.id.as_str()))
                .cloned(),
        );
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
        #[cfg(test)]
        test_crash_point("profile-move-journal");
        // The durable intent precedes every namespace publication.
        if let Some(before_commit) = before_commit.take() {
            before_commit(&moved)?;
        }
        if let Err(error) = source_admission
            .validate(&source_instances)
            .and_then(|()| target_admission.validate(&target_instances))
        {
            if existing_journal.is_none() {
                journal
                    .consume()
                    .context("removing an unadmitted profile move intent")?;
            }
            return Err(error);
        }

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
        // An external resource owner settles its own intent after publication.
        if existing_journal.is_none() {
            if let Err(error) = journal.consume() {
                tracing::warn!(
                    target: "session.store",
                    error = %error,
                    "completed profile move could not consume its journal; recovery will discard it"
                );
            }
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
    pub(crate) conflicts: Vec<String>,
    pub(crate) recovery_needed: bool,
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

pub(crate) fn inspect_profile_duplicates(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> ReconciliationOutcome {
    reconcile_profile_duplicates_inner(loaded, storages, false)
}

/// Detect duplicates across the loaded profiles, run journal-guided repair for the cases
/// with durable evidence, and return reports for whatever remains ambiguous.
pub(crate) fn reconcile_profile_duplicates(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> ReconciliationOutcome {
    reconcile_profile_duplicates_inner(loaded, storages, true)
}

fn reconcile_profile_duplicates_inner(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
    recover: bool,
) -> ReconciliationOutcome {
    let mut outcome = ReconciliationOutcome::default();
    // Normalize to a name-sorted view so report and copy ordering are
    // deterministic regardless of how the caller iterates its storages.
    let mut normalized: Vec<(&str, &[Instance])> = loaded.to_vec();
    normalized.sort_by(|left, right| left.0.cmp(right.0));
    let duplicated = !detect_duplicate_ids(normalized.iter().copied()).is_empty();
    let scan = super::move_journal::scan();
    outcome.recovery_needed = !scan.entries.is_empty() || !scan.unreadable_dirs.is_empty();
    let mut opaque_failure = !scan.unreadable_dirs.is_empty();
    for (dir, error) in scan.unreadable_dirs {
        outcome.conflicts.push(format!(
            "Move intent directory {} is unreadable: {error}; inspect with `aoe intents list` before resolving its entries",
            dir.display()
        ));
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
                outcome.conflicts.push(format!(
                    "Move intent conflict at {}: {reason}; inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`",
                    path.display(), shell_escape_script_word(&path.to_string_lossy())
                ));
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
        for (path, mut entry) in valid_entries {
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
            if let super::move_journal::MoveAuthority::LegacyConflict { reason, .. } =
                &entry.authority
            {
                outcome.conflicts.push(format!(
                    "Move intent conflict at {}: {reason}; resources were preserved. Inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`",
                    path.display(), shell_escape_script_word(&path.to_string_lossy())
                ));
                blocked_ids.extend(entry.ids.iter().cloned());
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
            if entry_age(&entry) > MOVE_JOURNAL_MAX_AGE && entry.is_arbitration_evidence() {
                mark_unusable_journal_entry(&path);
                blocked_ids.extend(entry.ids.iter().cloned());
                outcome.conflicts.push(format!("Move intent conflict at {}: expired arbitration evidence; inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`", path.display(), shell_escape_script_word(&path.to_string_lossy())));
                tracing::error!(
                    target: "session.store",
                    path = %path.display(),
                    ids = %entry.ids.join(","),
                    "expired move journal entry is insufficient evidence for arbitration; duplicates stay surfaced"
                );
                continue;
            }
            if !recover {
                let reason = match &entry.authority {
                    super::move_journal::MoveAuthority::Resource { intent } => {
                        intent.conflict.as_deref()
                    }
                    super::move_journal::MoveAuthority::Cleanup { conflict, .. } => {
                        conflict.as_deref()
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    outcome.conflicts.push(format!(
                        "Move intent conflict at {}: {reason}; resources were preserved. Inspect with `aoe intents list`",
                        path.display()
                    ));
                }
                blocked_ids.extend(entry.ids.iter().cloned());
                continue;
            }
            match repair_journal_entry(&mut entry, storages, &path) {
                Ok(true) => {
                    outcome.repaired = true;
                    tracing::info!(
                        target: "session.store",
                        ids = %entry.ids.join(","),
                        "reconciled interrupted profile move from its journal"
                    );
                }
                Ok(false) => {
                    let reason = match &entry.authority {
                        super::move_journal::MoveAuthority::LegacyConflict { reason, .. } => {
                            Some(reason.as_str())
                        }
                        super::move_journal::MoveAuthority::Resource { intent } => {
                            intent.conflict.as_deref()
                        }
                        super::move_journal::MoveAuthority::Cleanup { conflict, .. } => {
                            conflict.as_deref()
                        }
                        _ => None,
                    };
                    if let Some(reason) = reason {
                        outcome.conflicts.push(format!("Move intent conflict at {}: {reason}; resources were preserved. Inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`", path.display(), shell_escape_script_word(&path.to_string_lossy())));
                    }
                    blocked_ids.extend(entry.ids.iter().cloned());
                    tracing::debug!(
                        target: "session.store",
                        path = %path.display(),
                        "newer unresolved move intent blocks older overlapping journals"
                    );
                }
                Err(error) => {
                    outcome.conflicts.push(format!(
                        "Move intent conflict at {}: {error:#}; inspect with `aoe intents list`; clear with `aoe intents clear {} --actors-stopped`",
                        path.display(), shell_escape_script_word(&path.to_string_lossy())
                    ));
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

/// Hold each distinct save mutex, then each physical storage flock in a fixed order.
pub(crate) fn with_storages_locked_with_ownership<R>(
    ownership: &OwnershipGuard,
    storages: &[Storage],
    f: impl FnOnce() -> R,
) -> Result<R> {
    ownership.verify()?;
    let mut sorted = storages
        .iter()
        .map(|storage| {
            let dir = storage
                .sessions_path
                .parent()
                .ok_or_else(|| anyhow!("sessions path has no parent"))?;
            storage.verify_profile_available()?;
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
        .map(|storage| lock_save_mutex(&storage.save_lock, ownership.nonblocking))
        .collect::<Result<_>>()?;
    let dirs: Vec<&Path> = sorted
        .iter()
        .map(|(_, storage)| storage.sessions_path.parent().unwrap())
        .collect();
    let _transition_flocks =
        acquire_transition_flocks_for_profile_dirs(&dirs, ownership.nonblocking)?;
    let files = dirs
        .into_iter()
        .map(|dir| {
            let (file, path) = open_existing_storage_lock_file(dir, STORAGE_LOCK_FILENAME)?;
            OpenStorageLock::new(file, path)
        })
        .collect::<Result<Vec<_>>>()?;
    let _flocks = acquire_open_storage_flocks(files, ownership.nonblocking)?;
    for storage in storages {
        storage.verify_profile_available()?;
    }
    Ok(f())
}

fn with_two_storage_locks<F, R>(
    ownership: &OwnershipGuard,
    source: &Storage,
    target: &Storage,
    f: F,
) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    ownership.verify()?;
    source.verify_profile_available()?;
    target.verify_profile_available()?;
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
    let _mutexes = lock_two_save_mutexes(source, target, ownership.nonblocking)?;
    let (first, second) = (source, target);
    let first_dir = first.sessions_path.parent().unwrap();
    let second_dir = second.sessions_path.parent().unwrap();
    let _transition_flocks = acquire_transition_flocks_for_profile_dirs(
        &[first_dir, second_dir],
        ownership.nonblocking,
    )?;
    let (first_file, first_path) =
        open_existing_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
    let (second_file, second_path) =
        open_existing_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
    if same_filesystem_identity(&first_file.metadata()?, &second_file.metadata()?) {
        anyhow::bail!("source and target resolve to the same physical storage lock");
    }
    let _flocks = acquire_two_open_storage_flocks(
        OpenStorageLock::new(first_file, first_path)?,
        OpenStorageLock::new(second_file, second_path)?,
        ownership.nonblocking,
    )?;
    source.verify_profile_available()?;
    target.verify_profile_available()?;
    f()
}

pub(crate) fn abandon_resource_journal_with_ownership(
    root: &OwnershipGuard,
    journal: &super::move_journal::JournalTransaction,
    mut entry: super::move_journal::MoveJournalEntry,
    source: Result<Storage>,
) -> Result<bool> {
    use super::move_journal::{MoveAuthority, ProfileAuthority};
    root.require_exclusive()?;
    let MoveAuthority::Resource { intent } = &entry.authority else {
        anyhow::bail!("journal has no resource authority");
    };
    let storage = match source {
        Ok(storage) => Some(storage),
        Err(error) => {
            let directory = intent
                .owner
                .profile
                .sessions_path
                .parent()
                .context("resource profile path has no parent")?;
            anyhow::ensure!(
                matches!(fs::symlink_metadata(directory), Err(missing) if missing.kind() == std::io::ErrorKind::NotFound),
                "original resource profile is unavailable: {error:#}"
            );
            None
        }
    };
    let _identity = acquire_session_identity_lock_with_ownership(root)?;
    let _lifecycle = storage
        .as_ref()
        .map(|storage| {
            storage.acquire_instance_lifecycle_lock_with_ownership(root, &intent.owner.id)
        })
        .transpose()?;
    let mut release = false;
    if let Some(storage) = &storage {
        let profile = ProfileAuthority::capture(storage)?;
        anyhow::ensure!(
            intent.owner.profile == profile,
            "resource profile authority changed"
        );
        let rows = storage.load_strict_for_worktree_ownership()?;
        match rows.iter().find(|row| row.id == intent.owner.id) {
            Some(row) => {
                anyhow::ensure!(
                    intent.owner.matches(&profile, row),
                    "resource owner authority changed"
                );
                if row.lifecycle_reservation.is_some() {
                    anyhow::ensure!(
                        intent.row_is_reserved(&profile, row, journal.id()),
                        "resource reservation authority changed"
                    );
                    release = true;
                }
            }
            None => anyhow::ensure!(
                intent.operation == super::LifecycleOperation::Purge,
                "resource owner disappeared"
            ),
        }
    }
    let MoveAuthority::Resource { intent } = &mut entry.authority else {
        unreachable!()
    };
    if !intent.abandoned {
        intent.abandoned = true;
        intent.conflict = None;
        journal.rewrite(&entry)?;
    }
    if release {
        let storage = storage.as_ref().expect("validated resource profile");
        let MoveAuthority::Resource { intent } = &entry.authority else {
            unreachable!()
        };
        storage.update_with_intent(root, journal, |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == intent.owner.id)
                .context("resource owner disappeared")?;
            anyhow::ensure!(
                intent.row_is_reserved(&ProfileAuthority::capture(storage)?, row, journal.id()),
                "resource reservation authority changed"
            );
            row.release_lifecycle_reservation_if_owned(intent.operation, intent.owner.generation);
            Ok(())
        })?;
    }
    journal.consume()?;
    Ok(true)
}

pub(crate) fn recover_cleanup_journal(
    journal: &super::move_journal::JournalTransaction,
    mut entry: super::move_journal::MoveJournalEntry,
) -> Result<bool> {
    use super::move_journal::{EffectPhase, MoveAuthority};
    let root = acquire_ownership_lock()?;
    root.require_exclusive()?;
    let MoveAuthority::Cleanup {
        effects,
        abandoned,
        conflict,
        ..
    } = &mut entry.authority
    else {
        anyhow::bail!("journal has no cleanup authority");
    };
    if *abandoned {
        journal.consume()?;
        return Ok(true);
    }
    let complete = effects.iter().all(|record| match record.phase {
        EffectPhase::Prepared => record.before.is_none() && record.after.is_none(),
        EffectPhase::Applied => record
            .after
            .as_ref()
            .is_some_and(|after| record.effect.goal_reached(after).unwrap_or(false)),
        _ => false,
    });
    if complete {
        journal.consume()?;
        return Ok(true);
    }
    for record in effects {
        if matches!(record.phase, EffectPhase::Prepared | EffectPhase::Applied) {
            let verified = match record.phase {
                EffectPhase::Prepared => record.before.is_none() && record.after.is_none(),
                EffectPhase::Applied => record
                    .after
                    .as_ref()
                    .is_some_and(|after| record.effect.goal_reached(after).unwrap_or(false)),
                _ => unreachable!(),
            };
            if !verified {
                record.phase = EffectPhase::Conflict;
            }
        }
    }
    *conflict = Some("cleanup has an unfinished or unverified external effect; stop all actors before clearing it".into());
    journal.rewrite(&entry)?;
    Ok(false)
}

fn recover_resource_journal(
    journal: super::move_journal::JournalTransaction,
    mut entry: super::move_journal::MoveJournalEntry,
    source: Result<Storage>,
) -> Result<bool> {
    use super::move_journal::{EffectPhase, MoveAuthority};
    let MoveAuthority::Resource { intent } = &mut entry.authority else {
        anyhow::bail!("journal has no resource authority")
    };
    if intent.abandoned {
        let root = acquire_ownership_lock()?;
        return abandon_resource_journal_with_ownership(&root, &journal, entry, source);
    }
    if intent.phase == EffectPhase::Committed {
        anyhow::ensure!(
            intent
                .effects
                .iter()
                .all(|effect| effect.phase == EffectPhase::Applied
                    || (intent.rollback
                        && effect.phase == EffectPhase::Prepared
                        && effect.before.is_none())),
            "committed intent has unfinished effects"
        );
        let _root = acquire_ownership_lock()?;
        journal.consume()?;
        return Ok(true);
    }
    let pending = intent
        .effects
        .iter()
        .any(|effect| effect.phase == EffectPhase::Pending);
    if pending
        || intent
            .effects
            .iter()
            .any(|effect| effect.phase == EffectPhase::Conflict)
    {
        let _root = acquire_ownership_lock()?;
        intent.phase = EffectPhase::Conflict;
        let reason = if pending {
            "producer ended while an external effect was pending; its child may still be running"
        } else {
            "a completed effect could not be observed; its resources require conflict resolution"
        };
        intent.conflict = Some(reason.to_owned());
        journal.rewrite(&entry)?;
        anyhow::bail!("{reason}");
    }
    let source = source.context("original resource profile is unavailable")?;
    super::claim::ResourceReservation::from_recovery(&source, journal, entry)?.recover()
}

/// Apply the winner policy to one journal entry.
fn repair_journal_entry(
    entry: &mut super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
) -> Result<bool> {
    repair_journal_entry_with_sync(entry, storages, journal_path, sync_parent_directory)
}

fn repair_journal_entry_with_sync<S>(
    entry: &mut super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
    sync: S,
) -> Result<bool>
where
    S: FnMut(&Path) -> Result<()>,
{
    let Some((journal, fresh)) = super::move_journal::try_recovery_lease(journal_path)? else {
        return Ok(false);
    };
    if matches!(
        fresh.authority,
        super::move_journal::MoveAuthority::Cleanup { .. }
    ) {
        return Ok(false);
    }
    if let super::move_journal::MoveAuthority::Resource { intent } = &fresh.authority {
        let source = (|| {
            for (_, storage) in storages {
                if intent.owner.profile.matches(storage)? {
                    return Ok((*storage).clone());
                }
            }
            anyhow::bail!("original resource profile is unavailable")
        })();
        return recover_resource_journal(journal, fresh, source);
    }
    *entry = fresh;
    repair_metadata_entry_with_transaction(entry, storages, &journal, sync)
}

fn repair_metadata_entry_with_transaction<S>(
    entry: &super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal: &super::move_journal::JournalTransaction,
    mut sync: S,
) -> Result<bool>
where
    S: FnMut(&Path) -> Result<()>,
{
    use super::move_journal::{row_matches_delta, MoveAuthority};
    let journal_path = &journal.path;
    if let MoveAuthority::LegacyConflict { reason, .. } = &entry.authority {
        tracing::error!(target: "session.store", path = %journal_path.display(), %reason, "legacy move intent requires manual conflict resolution; no row changed");
        return Ok(false);
    }
    let MoveAuthority::Metadata {
        source,
        target,
        rows,
    } = &entry.authority
    else {
        unreachable!();
    };
    let ownership = acquire_ownership_lock()?;
    if !(source.profile == entry.source_profile
        && target.profile == entry.target_profile
        && super::deletion::resolve_claim_path(&entry.source_sessions_path).as_ref()
            == Some(&source.sessions_path)
        && super::deletion::resolve_claim_path(&entry.target_sessions_path).as_ref()
            == Some(&target.sessions_path)
        && rows.len() == entry.ids.len()
        && entry.ids.iter().all(|id| {
            super::validate_instance_id(id).is_ok()
                && entry.ids.iter().filter(|other| *other == id).count() == 1
                && rows
                    .iter()
                    .filter(|row| &row.before.id == id && row.after.id == row.before.id)
                    .count()
                    == 1
        }))
    {
        mark_unusable_journal_entry(journal_path);
        tracing::error!(target: "session.store", path = %journal_path.display(), "move journal authority does not match its declared batch");
        return Ok(false);
    }
    let mut source_storage = None;
    let mut target_storage = None;
    for (_, storage) in storages {
        if source.matches(storage)? {
            source_storage = Some(*storage);
        }
        if target.matches(storage)? {
            target_storage = Some(*storage);
        }
    }
    let (Some(source_storage), Some(target_storage)) = (source_storage, target_storage) else {
        return Ok(false);
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

    let _identity_lock = acquire_session_identity_lock_with_ownership(&ownership)?;
    let mut ids_sorted = entry.ids.clone();
    ids_sorted.sort();
    ids_sorted.dedup();
    let mut guards = Vec::with_capacity(ids_sorted.len() * 2);
    for id in &ids_sorted {
        guards.push(acquire_session_title_lock_with_ownership(&ownership, id)?);
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
        guards.push(source_storage.acquire_instance_lifecycle_lock_with_ownership(&ownership, id)?);
        if source_scope != target_scope {
            guards.push(
                target_storage.acquire_instance_lifecycle_lock_with_ownership(&ownership, id)?,
            );
        }
    }

    with_two_storage_locks(&ownership, source_storage, target_storage, || {
        let source_instances = source_storage.load_strict_for_worktree_ownership()?;
        let target_instances = target_storage.load_strict_for_worktree_ownership()?;
        let plan = crate::session::GroupMovePlan {
            source_path: entry.group_move_source_path.clone(),
            target_path: entry.group_move_target_path.clone(),
            move_subtree: entry.group_move_subtree,
        };
        let mut source_losers = Vec::new();
        for delta in rows {
            let mut source_rows = source_instances
                .iter()
                .filter(|row| row.id == delta.before.id);
            let mut target_rows = target_instances
                .iter()
                .filter(|row| row.id == delta.after.id);
            let pair = (source_rows.next(), target_rows.next());
            if source_rows.next().is_some() || target_rows.next().is_some() {
                return Ok(false);
            }
            let valid = match pair {
                (Some(before), Some(after)) => {
                    if row_matches_delta(before, delta, false)
                        && row_matches_delta(after, delta, true)
                        && super::move_journal::source_changes_preserved(
                            &delta.before,
                            before,
                            after,
                        )?
                    {
                        source_losers.push(delta.before.id.clone());
                        true
                    } else {
                        false
                    }
                }
                (Some(before), None) => row_matches_delta(before, delta, false),
                (None, Some(after)) => row_matches_delta(after, delta, true),
                (None, None) => true,
            };
            if !valid {
                tracing::error!(target: "session.store", path = %journal_path.display(), session = %delta.before.id, "move intent authority was superseded; retaining all copies");
                return Ok(false);
            }
        }
        if source_losers.is_empty() {
            sync_repaired_profile_durably(source_storage, &mut sync)?;
            sync_repaired_profile_durably(target_storage, &mut sync)?;
            journal.consume()?;
            return Ok(true);
        }

        source_storage.update_under_lock(Some(journal), None, |instances, groups| {
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
        sync_repaired_profile_durably(target_storage, &mut sync)?;
        #[cfg(test)]
        test_crash_point("profile-repair-source-written");
        journal.consume()?;
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
const MIGRATION_BACKUP_MARKER: &str = ".pre-migration-";

/// Back up a file the move-journal repair is about to rewrite. `Err` means no
/// durable copy landed, and the caller is mid-repair, so it must stop. A `path`
/// holding nothing to copy is not a failure: the repair backs up a
/// `groups.json` that many profiles never had.
pub(crate) fn backup_before_repair(path: &Path) -> Result<()> {
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
pub(crate) fn reconcile_groups_after_repair(
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

    #[test]
    #[serial]
    fn captured_storage_rejects_work_after_private_namespace_restoration() {
        let outer = tempdir().unwrap();
        let _outer = isolate_app_dir_at(outer.path());
        let outer_app = super::super::get_app_dir().unwrap();
        let inner = tempdir().unwrap();
        let inner_guard = isolate_app_dir_at(inner.path());
        let storage = Storage::new_unwatched("inner").unwrap();
        storage
            .update(|rows, _| {
                rows.push(Instance::new("kept", "/kept"));
                Ok(())
            })
            .unwrap();
        let original = std::fs::read(storage.sessions_path()).unwrap();
        let path = storage.sessions_path().to_path_buf();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            storage.update(|rows, _| {
                rows.clear();
                Ok(())
            })
        });
        ready_rx.recv().unwrap();
        drop(inner_guard);
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert!(!outer_app.join(".workspace-claim.lock").exists());
        assert!(!outer_app.join("profiles").exists());
    }
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
    #[serial]
    fn profile_batch_move_rejects_collision_and_merges_fresh_source() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_for_test_profile("move-source");
        let target = Storage::new_for_test_profile("move-target");
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
        let rejected = source.move_instances_to_with_ownership(
            &acquire_ownership_lock()?,
            &target,
            &changes,
            &group_move,
            |existing, candidates| {
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
            },
        );
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
        let moved = source.move_instances_to_with_ownership(
            &acquire_ownership_lock()?,
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
    fn profile_move_rejects_target_collision_without_changing_rows() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_for_test_profile("effect-source");
        let target = Storage::new_for_test_profile("effect-target");
        let before = Instance::new("collision", "/repo/collision");
        source.update(|instances, _groups| {
            instances.push(before.clone());
            Ok(())
        })?;
        target.update(|instances, _groups| {
            instances.push(Instance::new("collision", "/repo/collision/"));
            Ok(())
        })?;
        let source_rows = serde_json::to_value(source.load()?)?;
        let target_rows = serde_json::to_value(target.load()?)?;
        let ownership = acquire_ownership_lock()?;
        let result = source.move_instance_to_with_ownership(
            &ownership,
            &target,
            (&before, &before),
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
            None,
        );
        drop(ownership);

        assert!(result.is_err());
        assert_eq!(serde_json::to_value(source.load()?)?, source_rows);
        assert_eq!(serde_json::to_value(target.load()?)?, target_rows);
        Ok(())
    }

    #[test]
    #[serial]
    fn profile_move_transfers_explicit_empty_group_metadata() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_for_test_profile("empty-source");
        let target = Storage::new_for_test_profile("empty-target");
        source.update(|_instances, groups| {
            let mut group = Group::new("empty", "empty");
            group.collapsed = true;
            group.archived_at = Some(chrono::Utc::now());
            groups.push(group);
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        let moved = source.move_instances_to_with_ownership(
            &acquire_ownership_lock()?,
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
    #[serial]
    fn profile_group_move_rejects_fresh_unplanned_member() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_for_test_profile("member-source");
        let target = Storage::new_for_test_profile("member-target");
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
            .move_instances_to_with_ownership(
                &acquire_ownership_lock()?,
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
    #[serial]
    fn profile_move_syncs_resolved_symlink_target_parent() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let source = Storage::new_for_test_profile("symlink-source");
        let target = Storage::new_for_test_profile("symlink-target");
        let resolved_sessions_dir = temp.path().join("resolved-sessions");
        let resolved_groups_dir = temp.path().join("resolved-groups");
        let target_dir = target.sessions_path().parent().unwrap();
        fs::create_dir_all(&resolved_sessions_dir)?;
        fs::create_dir_all(&resolved_groups_dir)?;
        let target_link = target.sessions_path();
        let target_groups_link = target_dir.join("groups.json");
        let resolved_sessions = resolved_sessions_dir.join("sessions.json");
        let resolved_groups = resolved_groups_dir.join("groups.json");
        fs::write(&resolved_sessions, b"[]")?;
        fs::write(&resolved_groups, b"[]")?;
        symlink(&resolved_sessions, target_link)?;
        symlink(&resolved_groups, &target_groups_link)?;
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
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("inode")?;
        let source_dir = source.sessions_path().parent().unwrap();
        let target_dir = target.sessions_path().parent().unwrap();
        target.update(|_instances, groups| {
            groups.push(Group::new("target", "target"));
            Ok(())
        })?;
        let target_lock = target_dir.join(STORAGE_LOCK_FILENAME);
        fs::remove_file(&target_lock)?;
        fs::hard_link(source_dir.join(STORAGE_LOCK_FILENAME), &target_lock)?;
        let try_move = || {
            let ownership = acquire_ownership_lock().expect("fixture ownership fence");
            source
                .move_instance_to_with_ownership(
                    &ownership,
                    &target,
                    (&before, &before),
                    false,
                    |_, _| Ok(()),
                    None,
                )
                .expect_err("shared storage files must be rejected before locking")
        };

        let _ = try_move();
        assert_eq!(source.load()?.len(), 1);
        assert!(target.load()?.is_empty());

        fs::remove_file(&target_lock)?;
        fs::File::create(&target_lock)?;
        let target_groups = target_dir.join("groups.json");
        fs::remove_file(&target_groups)?;
        fs::hard_link(source_dir.join("groups.json"), &target_groups)?;
        let _ = try_move();
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
        let source = Storage::new_for_test_profile(&format!("{tag}-source"));
        let target = Storage::new_for_test_profile(&format!("{tag}-target"));
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
    fn recovery_keeps_forked_source_annotations_when_target_has_other_value() -> Result<()> {
        for clears_color in [false, true] {
            let (_temp, _guard, source, target, _, _) =
                setup_recovery_env("source-annotation-fork")?;
            source.update(|rows, _| {
                rows[0].color = Some("blue".into());
                Ok(())
            })?;
            run_crashing_move(&source, &target, "profile-move-target");
            source.update(|rows, _| {
                rows[0].color = (!clears_color).then(|| "green".into());
                Ok(())
            })?;
            target.update(|rows, _| {
                rows[0].color = Some("red".into());
                Ok(())
            })?;
            let source_bytes = fs::read(source.sessions_path())?;
            let target_bytes = fs::read(target.sessions_path())?;
            let outcome = reconcile_loaded(&[&source, &target]);
            assert!(!outcome.repaired);
            assert_eq!(outcome.reports.len(), 1);
            assert_eq!(fs::read(source.sessions_path())?, source_bytes);
            assert_eq!(fs::read(target.sessions_path())?, target_bytes);
            assert_eq!(journal_entry_count(&source), 1);
        }
        Ok(())
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
        fn record(entry: &move_journal::MoveJournalEntry) -> Result<()> {
            move_journal::record(entry).map(drop)
        }
        fn copy_and_record(
            edit: fn(&mut move_journal::MoveJournalEntry),
        ) -> impl Fn(&Storage, &Storage, &Instance) -> Result<()> {
            move |a, b, before| {
                push_copy(b, before)?;
                let mut entry = fresh_journal_entry(a, b, &before.id);
                edit(&mut entry);
                record(&entry)
            }
        }
        let legacy: Arrange = |_, b, before| push_copy(b, before);
        let wrong_version: Arrange = |a, b, before| {
            push_copy(b, before)?;
            let mut entry = fresh_journal_entry(a, b, &before.id);
            let path = move_journal::record(&entry)?;
            entry.version = move_journal::MOVE_JOURNAL_VERSION + 1;
            fs::write(path, serde_json::to_vec_pretty(&entry)?)?;
            Ok(())
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
            record(&entry)
        };
        let aliased: Arrange = |a, b, before| {
            let mut entry = fresh_journal_entry(a, b, &before.id);
            entry.target_profile = a.profile().to_string();
            entry.target_sessions_path = a.sessions_path().to_path_buf();
            record(&entry)
        };
        let target_duplicate: Arrange = |a, b, before| {
            push_copy(b, before)?;
            push_copy(b, before)?;
            record(&fresh_journal_entry(a, b, &before.id))
        };
        let same_profile: Arrange = |a, _, before| push_copy(a, before);
        let newer_unresolved: Arrange = |a, b, before| {
            push_copy(b, before)?;
            let mut older = fresh_journal_entry(a, b, &before.id);
            older.created_at_epoch_ms = now_ms() - 60_000;
            record(&older)?;
            let mut newer = fresh_journal_entry(b, a, &before.id);
            newer.target_profile = "missing-profile".to_string();
            newer.target_sessions_path = a.sessions_path().with_file_name("missing.json");
            record(&newer)
        };
        let newer_opaque: Arrange = |a, b, before| {
            push_copy(b, before)?;
            let mut older = fresh_journal_entry(a, b, &before.id);
            older.created_at_epoch_ms = now_ms() - 60_000;
            record(&older)?;
            let journal_dir = move_journal::journal_dir()?;
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
            record(&j1)?;
            record(&j2)?;
            record(&j3)
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

    #[cfg(unix)]
    #[test]
    fn metadata_recovery_accepts_aliased_header_paths_without_changing_authority() -> Result<()> {
        let (temp, _guard, source, target, before, _after) = setup_recovery_env("alias-recovery")?;
        let alias = temp.path().join("route-alias");
        std::os::unix::fs::symlink(fs::canonicalize(temp.path())?, &alias)?;
        let aliased_source = Storage::new_for_test_path(
            source.profile(),
            alias.join(source.sessions_path().strip_prefix(temp.path())?),
        );
        let aliased_target = Storage::new_for_test_path(
            target.profile(),
            alias.join(target.sessions_path().strip_prefix(temp.path())?),
        );
        run_crashing_move(&aliased_source, &aliased_target, "profile-move-target");
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        let mut scan = move_journal::scan();
        assert_eq!(scan.entries.len(), 1);
        let (journal_path, parsed) = scan.entries.remove(0);
        let entry = parsed.map_err(anyhow::Error::msg)?;
        let move_journal::MoveAuthority::Metadata {
            source: source_authority,
            target: target_authority,
            ..
        } = &entry.authority
        else {
            panic!("the real profile move must record metadata authority");
        };
        assert_ne!(entry.source_sessions_path, source_authority.sessions_path);
        assert_ne!(entry.target_sessions_path, target_authority.sessions_path);
        assert!(source_authority.matches(&source)?);
        assert!(target_authority.matches(&target)?);
        let outcome = reconcile_loaded(&[&source, &target]);
        assert!(outcome.repaired);
        assert!(outcome.reports.is_empty());
        assert!(outcome.conflicts.is_empty());
        assert!(!journal_path.exists());
        assert!(source.load()?.is_empty());
        let winners = target.load()?;
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].id, before.id);
        assert_eq!(winners[0].created_at, before.created_at);
        assert_eq!(winners[0].project_path, before.project_path);
        assert_eq!(winners[0].group_path, "moved");
        assert_eq!(winners[0].lifecycle_generation, before.lifecycle_generation);
        assert!(winners[0].lifecycle_reservation.is_none());
        assert!(!source
            .load_with_groups()?
            .1
            .iter()
            .any(|group| group.path == "work"));
        assert!(target
            .load_with_groups()?
            .1
            .iter()
            .any(|group| group.path == "moved"));
        let repeated = reconcile_loaded(&[&source, &target]);
        assert!(!repeated.repaired);
        assert!(repeated.reports.is_empty());
        assert!(repeated.conflicts.is_empty());
        assert!(source.load()?.is_empty());
        assert_eq!(target.load()?.len(), 1);
        assert!(move_journal::scan().entries.is_empty());
        Ok(())
    }

    #[test]
    fn resolve_miss_is_transient_not_permanent() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("resolvemiss")?;
        push_copy(&target, &before)?;
        move_journal::record(&fresh_journal_entry(&source, &target, &before.id))?;

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
        let vanished = Instance::new("vanished", "/repo/vanished");
        let vanished_id = vanished.id.clone();
        push_copy(&source, &vanished)?;
        push_copy(&target, &before)?;
        let mut entry = fresh_journal_entry(&source, &target, &before.id);
        entry.ids.push(vanished_id.clone());
        entry.ids.sort();
        if let move_journal::MoveAuthority::Metadata { rows, .. } = &mut entry.authority {
            rows.push(journal_delta(&source, &target, &vanished_id));
        }
        source.update(|rows, _| {
            rows.retain(|row| row.id != vanished_id);
            Ok(())
        })?;
        move_journal::record(&entry)?;

        let outcome = reconcile_loaded(&[&source, &target]);

        assert!(outcome.repaired, "the duplicated sibling must arbitrate");
        assert!(outcome.reports.is_empty());
        assert!(
            !source.load()?.iter().any(|row| row.id == before.id),
            "loser copy removed"
        );
        let winners = target.load()?;
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].id, before.id);
        assert!(!source.load()?.iter().any(|row| row.id == vanished_id));
        assert!(!winners.iter().any(|row| row.id == vanished_id));
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
        let ownership = acquire_ownership_read()?;

        let (contended, entered_early) =
            with_two_storage_locks(&ownership, &source, &target, || {
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
    fn post_repair_load_error_prevents_home_reload_and_keeps_report() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) =
            setup_recovery_env("reload-fallback")?;
        push_copy(&target, &before)?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        move_journal::record(&entry)?;
        let bad = Storage::new_for_test_profile("bad");
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

    fn journal_entry_paths(source: &Storage) -> impl Iterator<Item = PathBuf> + '_ {
        move_journal::scan()
            .entries
            .into_iter()
            .filter_map(move |(path, _)| {
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).ok()?;
                (value
                    .get("source_sessions_path")
                    .and_then(serde_json::Value::as_str)
                    == source.sessions_path().to_str())
                .then_some(path)
            })
    }

    fn journal_entry_count(source: &Storage) -> usize {
        journal_entry_paths(source).count()
    }

    fn journal_entry_scan_ids(source: &Storage) -> Vec<String> {
        move_journal::scan()
            .entries
            .into_iter()
            .filter_map(|(_, parsed)| parsed.ok())
            .filter(|entry| entry.source_sessions_path == source.sessions_path())
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
            move_journal::record(&entry)?;

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
    fn unresolved_move_refuses_a_competing_move_and_remains_recoverable() -> Result<()> {
        let (_temp, _guard, a, b, before, _after) = setup_recovery_env("chain")?;
        let c = Storage::new_for_test_profile("chain-third");
        c.update(|_, _| Ok(()))?;
        run_crashing_move(&a, &b, "profile-move-target");
        assert_eq!(journal_entry_count(&a), 1);
        let (a_rows, a_groups) = a.load_with_groups()?;
        let (b_rows, b_groups) = b.load_with_groups()?;
        let (c_rows, c_groups) = c.load_with_groups()?;
        assert_eq!(a_rows.len(), 1);
        assert_eq!(b_rows.len(), 1);
        assert!(c_rows.is_empty());
        let reverse_before = b_rows[0].clone();
        let mut reverse_after = reverse_before.clone();
        reverse_after.group_path = "work".into();
        let refused = b.move_instances_to_with_ownership(
            &acquire_ownership_lock()?,
            &c,
            &[(reverse_before, reverse_after)],
            &GroupMovePlan::single("moved", "work"),
            |_, _| Ok(()),
        );
        assert!(
            refused.is_err(),
            "an unresolved move must reserve its session"
        );
        for (storage, expected_rows, expected_groups) in [
            (&a, &a_rows, &a_groups),
            (&b, &b_rows, &b_groups),
            (&c, &c_rows, &c_groups),
        ] {
            let (rows, groups) = storage.load_with_groups()?;
            assert_eq!(rows.len(), expected_rows.len());
            assert_eq!(&groups, expected_groups);
            for (row, expected) in rows.iter().zip(expected_rows) {
                assert_eq!(row.id, expected.id);
                assert_eq!(row.created_at, expected.created_at);
                assert_eq!(row.title, expected.title);
                assert_eq!(row.project_path, expected.project_path);
                assert_eq!(row.group_path, expected.group_path);
                assert_eq!(row.worktree_info, expected.worktree_info);
                assert_eq!(row.lifecycle_generation, expected.lifecycle_generation);
                assert_eq!(row.lifecycle_reservation, expected.lifecycle_reservation);
            }
        }
        assert_eq!(
            journal_entry_count(&a),
            1,
            "the unadmitted competing intent was retired"
        );

        let outcome = reconcile_loaded(&[&a, &b, &c]);

        assert!(outcome.repaired);
        assert!(outcome.reports.is_empty());
        assert!(outcome.conflicts.is_empty());
        assert!(
            a.load()?.is_empty(),
            "the original interrupted move source loses"
        );
        let winners = b.load()?;
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].id, before.id);
        assert_eq!(winners[0].created_at, before.created_at);
        assert_eq!(winners[0].project_path, before.project_path);
        assert_eq!(winners[0].group_path, "moved");
        assert_eq!(winners[0].lifecycle_generation, before.lifecycle_generation);
        assert!(winners[0].lifecycle_reservation.is_none());
        assert!(c.load()?.is_empty());
        assert_eq!(journal_entry_count(&a), 0);
        let repeated = reconcile_loaded(&[&a, &b, &c]);
        assert!(!repeated.repaired);
        assert!(repeated.reports.is_empty());
        assert!(repeated.conflicts.is_empty());
        assert!(a.load()?.is_empty());
        assert_eq!(b.load()?.len(), 1);
        assert!(c.load()?.is_empty());
        assert_eq!(journal_entry_count(&a), 0);
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
        move_journal::record(&entry)?;

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
        let journal_path = move_journal::record(&entry)?;
        let mut entry = serde_json::from_slice(&fs::read(&journal_path)?)?;
        let stores = stores(&[&source, &target]);
        let error = repair_journal_entry_with_sync(&mut entry, &stores, &journal_path, |_path| {
            Err(anyhow!("forced repaired-profile sync failure"))
        })
        .expect_err("failed durability barrier must fail recovery completion");
        assert!(error.to_string().contains("not made durable"));
        assert!(source.load()?.is_empty(), "repair row write reached disk");
        assert_eq!(journal_entry_count(&source), 1, "evidence must remain");

        let retry_error =
            repair_journal_entry_with_sync(&mut entry, &stores, &journal_path, |_path| {
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
            authority: move_journal::MoveAuthority::Metadata {
                source: move_journal::ProfileAuthority::capture(source).unwrap(),
                target: move_journal::ProfileAuthority::capture(target).unwrap(),
                rows: vec![journal_delta(source, target, id)],
            },
        }
    }

    fn journal_delta(source: &Storage, target: &Storage, id: &str) -> move_journal::RowDelta {
        let before = source
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .expect("original source row");
        let mut after = target
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| before.clone());
        after.source_profile = target.profile().to_owned();
        move_journal::RowDelta { before, after }
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
        journal_entry_paths(storage)
            .next()
            .expect("journal entry on disk")
    }

    #[test]
    #[serial_test::serial]
    fn creation_try_update_never_runs_closure_under_nested_contention() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("try-admission").unwrap();
        storage.update(|_, _| Ok(())).unwrap();
        let root = acquire_ownership_lock().unwrap();
        let mut called = false;
        let mutex = storage
            .save_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(storage
            .try_update_with_ownership(&root, |_, _| {
                called = true;
                Ok(())
            })
            .unwrap()
            .is_none());
        assert!(!called);
        drop(mutex);
        let profile = storage.sessions_path.parent().unwrap();
        for (directory, name) in [
            (
                root.app_dir(),
                crate::migrations::v027_isolate_sandbox_stores::LOCK,
            ),
            (profile, STORAGE_LOCK_FILENAME),
        ] {
            let holder = acquire_storage_flock(directory, name).unwrap();
            assert!(storage
                .try_update_with_ownership(&root, |_, _| {
                    called = true;
                    Ok(())
                })
                .unwrap()
                .is_none());
            assert!(!called);
            drop(holder);
        }
        let id = Instance::new("lifecycle", "/absent").id;
        let holder = storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &id)
            .unwrap();
        assert!(storage
            .try_acquire_instance_lifecycle_lock_with_ownership(&root, &id)
            .unwrap()
            .is_none());
        drop(holder);
        assert!(storage
            .try_acquire_instance_lifecycle_lock_with_ownership(&root, &id)
            .unwrap()
            .is_some());
        assert_eq!(
            storage
                .try_update_with_ownership(&root, |rows, _| {
                    called = true;
                    rows.push(Instance::new("admitted", "/absent"));
                    Ok(7)
                })
                .unwrap(),
            Some(7)
        );
        assert!(called);
        assert_eq!(storage.load().unwrap().len(), 1);
    }
}

//! File operations rooted at a directory descriptor.

use anyhow::{bail, Context, Result};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
use nix::sys::stat::{fstat, fstatat, mkdirat, FileStat, Mode};
use nix::unistd::{linkat, unlinkat, UnlinkatFlags};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

pub(crate) struct AnchoredDir {
    root: PathBuf,
    fd: OwnedFd,
}

pub(crate) struct FilePublication<'a> {
    pub(crate) staging: &'a AnchoredDir,
    pub(crate) validate: &'a dyn Fn() -> Result<()>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_SYNC_ONCE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

impl AnchoredDir {
    /// Anchor at `path`, whose ancestors are resolved the way any other caller resolves them and
    /// whose own leaf may not be a symlink.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let root = path.to_path_buf();
        for component in path.components() {
            if !matches!(
                component,
                Component::RootDir | Component::CurDir | Component::Normal(_)
            ) {
                bail!(
                    "anchored root contains a non-normal component: {}",
                    path.display()
                );
            }
        }
        let resolving = OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_RDONLY;
        let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
            // A bare root such as `/` or `.` has no leaf to guard.
            let fd = open(path, resolving, Mode::empty())
                .with_context(|| format!("opening anchored root {}", root.display()))?;
            return Ok(Self { root, fd });
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let parent_fd = open(parent, resolving, Mode::empty())
            .with_context(|| format!("opening anchored root parent {}", parent.display()))?;
        let fd = openat(
            &parent_fd,
            leaf,
            resolving | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .with_context(|| format!("opening anchored root {}", root.display()))?;
        Ok(Self { root, fd })
    }

    pub(crate) fn create(path: &Path) -> Result<Self> {
        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()))
            }
        }
        Self::open(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn ensure_dir(&self, relative: &Path) -> Result<PathBuf> {
        self.ensure_dir_fd(relative)?;
        Ok(self.root.join(relative))
    }

    pub(crate) fn child(&self, relative: &Path) -> Result<Self> {
        Ok(Self {
            root: self.root.join(relative),
            fd: self.open_dir(relative)?,
        })
    }

    pub(crate) fn create_child(&self, relative: &Path) -> Result<Self> {
        Ok(Self {
            root: self.root.join(relative),
            fd: self.ensure_dir_fd(relative)?,
        })
    }

    pub(crate) fn identity(&self) -> Result<(libc::dev_t, libc::ino_t)> {
        let stat = fstat(&self.fd)?;
        Ok((stat.st_dev, stat.st_ino))
    }

    pub(crate) fn sync(&self) -> Result<()> {
        #[cfg(test)]
        if FAIL_SYNC_ONCE.with(|path| {
            let mut path = path.borrow_mut();
            if path.as_deref() == Some(self.path()) {
                path.take();
                true
            } else {
                false
            }
        }) {
            bail!("injected anchored directory sync failure");
        }
        nix::unistd::fsync(&self.fd).context("syncing anchored directory")
    }

    fn ensure_dir_fd(&self, relative: &Path) -> Result<OwnedFd> {
        let components = normal_components(relative)?;
        let mut current = Self {
            root: self.root.clone(),
            fd: openat(
                &self.fd,
                ".",
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )?,
        };
        for component in &components {
            match mkdirat(&current.fd, component.as_os_str(), Mode::S_IRWXU) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(error) => return Err(error).context("creating anchored directory"),
            }
            let child = openat(
                &current.fd,
                component.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )
            .context("opening anchored directory component")?;
            current.sync()?;
            current.root.push(component);
            current.fd = child;
        }
        Ok(current.fd)
    }

    pub(crate) fn open_regular(&self, relative: &Path, max_bytes: usize) -> Result<Option<File>> {
        let (parent, leaf) = self.open_parent(relative)?;
        let fd = match openat(
            &parent,
            leaf.as_os_str(),
            OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) | Err(Errno::ELOOP) | Err(Errno::ENOTDIR) => return Ok(None),
            Err(error) => return Err(error).context("opening anchored file"),
        };
        let stat = fstat(&fd)?;
        if (stat.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFREG
            || stat.st_size < 0
            || u64::try_from(stat.st_size).unwrap_or(u64::MAX)
                > u64::try_from(max_bytes).unwrap_or(u64::MAX)
        {
            return Ok(None);
        }
        Ok(Some(File::from(fd)))
    }

    pub(crate) fn read_regular(
        &self,
        relative: &Path,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.open_regular(relative, max_bytes)? else {
            return Ok(None);
        };
        let mut bytes = Vec::with_capacity(max_bytes.min(4096));
        file.take(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// Create `relative` for writing, or `None` when an entry is already
    /// there. `O_EXCL | O_NOFOLLOW` so a planted symlink is never followed
    /// and an existing file is never truncated.
    pub(crate) fn create_new_regular(&self, relative: &Path) -> Result<Option<File>> {
        let (parent, leaf) = self.open_parent(relative)?;
        match openat(
            &parent,
            leaf.as_os_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::S_IRUSR | Mode::S_IWUSR,
        ) {
            Ok(fd) => Ok(Some(File::from(fd))),
            Err(Errno::EEXIST) => Ok(None),
            Err(error) => Err(error).context("creating anchored file"),
        }
    }

    pub(crate) fn read_dir(&self, relative: &Path, max_entries: usize) -> Result<Vec<OsString>> {
        let fd = self.open_dir(relative)?;
        let mut dir = Dir::from_fd(fd)?;
        let mut names = Vec::with_capacity(max_entries.min(64));
        for entry in dir.iter() {
            let Ok(entry) = entry else {
                continue;
            };
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if names.len() == max_entries {
                break;
            }
            names.push(OsString::from_vec(name.to_vec()));
        }
        Ok(names)
    }

    pub(crate) fn regular_modified(
        &self,
        relative: &Path,
    ) -> Result<Option<std::time::SystemTime>> {
        self.modified(relative, false)
    }

    pub(crate) fn directory_modified(
        &self,
        relative: &Path,
    ) -> Result<Option<std::time::SystemTime>> {
        self.modified(relative, true)
    }

    /// What `relative` names: `Some(true)` a regular file, `Some(false)` something that is not one,
    /// `None` nothing at all.
    pub(crate) fn regular_lookup(&self, relative: &Path) -> Result<Option<bool>> {
        let (parent, leaf) = self.open_parent(relative)?;
        match fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(
                (stat.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFREG,
            )),
            Err(Errno::ENOENT) => Ok(None),
            Err(error) => Err(error).context("inspecting anchored file"),
        }
    }

    pub(crate) fn regular_exists(&self, relative: &Path) -> bool {
        matches!(self.regular_lookup(relative), Ok(Some(true)))
    }

    /// Publish the staging file `from` at `to` and drop the staging name.
    /// `false` means something was already at `to` and was left untouched,
    /// which only happens when `replace` is unset.
    ///
    /// Makes a file visible only once it is complete, so a process killed
    /// mid-write leaves a staging name rather than a half file under the real
    /// one. Without `replace` this is `linkat`, not `renameat`, because rename
    /// replaces the destination: a writer that created `to` between a caller's
    /// existence check and this call would lose its file.
    /// `renameat2(RENAME_NOREPLACE)` would also answer, but it is Linux-only
    /// and this has to hold on macOS.
    pub(crate) fn publish_staged(&self, from: &Path, to: &Path, replace: bool) -> Result<bool> {
        let (from_parent, from_leaf) = self.open_parent(from)?;
        let (to_parent, to_leaf) = self.open_parent(to)?;
        if replace {
            renameat(
                &from_parent,
                from_leaf.as_os_str(),
                &to_parent,
                to_leaf.as_os_str(),
            )
            .context("replacing anchored file")?;
            return Ok(true);
        }
        let published = match linkat(
            &from_parent,
            from_leaf.as_os_str(),
            &to_parent,
            to_leaf.as_os_str(),
            AtFlags::empty(),
        ) {
            Ok(()) => true,
            Err(Errno::EEXIST) => false,
            Err(error) => return Err(error).context("publishing anchored file"),
        };
        self.remove_file(from)?;
        Ok(published)
    }
    /// Stat one direct child without following a link at its leaf, so a caller
    /// sees the entry the directory holds rather than what it resolves to.
    pub(crate) fn entry_stat(&self, leaf: &Path) -> Result<Option<FileStat>> {
        let components = normal_components(leaf)?;
        let [name] = components.as_slice() else {
            bail!("anchored entry is not a single component");
        };
        match fstatat(&self.fd, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(stat)),
            Err(Errno::ENOENT) => Ok(None),
            Err(error) => Err(error).context("inspecting anchored entry"),
        }
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative)?;
        match unlinkat(&parent, leaf.as_os_str(), UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(error) => Err(error).context("removing anchored file"),
        }
    }

    /// Publish a complete regular file. A seed never replaces any existing
    /// entry; refresh replaces the leaf, never follows a link at that leaf.
    /// Parent directories must already exist beneath this retained anchor.
    pub(crate) fn publish_file(
        &self,
        relative: &Path,
        reader: &mut impl Read,
        permissions: std::fs::Permissions,
        replace: bool,
        publication: Option<FilePublication<'_>>,
    ) -> Result<bool> {
        let (parent, leaf) = self.open_parent(relative)?;
        let staging = publication
            .as_ref()
            .map_or(&parent, |publication| &publication.staging.fd);
        let temporary = format!(".aoe-copy-{}", uuid::Uuid::new_v4());
        let fd = openat(
            staging,
            temporary.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .context("creating anchored file stage")?;
        let mut file = File::from(fd);
        let result = (|| -> Result<bool> {
            std::io::copy(reader, &mut file).context("copying anchored file stage")?;
            file.set_permissions(permissions)?;
            file.sync_all()?;
            if let Some(publication) = &publication {
                (publication.validate)()?;
            }
            if replace {
                nix::fcntl::renameat(staging, temporary.as_str(), &parent, leaf.as_os_str())?;
            } else {
                match crate::process::rename_exclusive(
                    staging,
                    temporary.as_ref(),
                    &parent,
                    leaf.as_os_str(),
                ) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Ok(false);
                    }
                    Err(error) => return Err(error).context("publishing anchored file seed"),
                }
            }
            if publication.is_some() {
                nix::unistd::fsync(staging)?;
            }
            nix::unistd::fsync(&parent)?;
            Ok(true)
        })();
        if !matches!(result, Ok(true)) {
            let _ = unlinkat(staging, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
        }
        result
    }

    /// The fully built stage remains available when another writer wins.
    pub(crate) fn publish_directory(
        &self,
        source: &AnchoredDir,
        stage: &Path,
        destination: &Path,
    ) -> Result<bool> {
        let (source_parent, source_leaf) = source.open_parent(stage)?;
        let (target_parent, target_leaf) = self.open_parent(destination)?;
        match crate::process::rename_exclusive(
            &source_parent,
            source_leaf.as_os_str(),
            &target_parent,
            target_leaf.as_os_str(),
        ) {
            Ok(()) => {
                nix::unistd::fsync(&source_parent)?;
                nix::unistd::fsync(&target_parent)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error).context("publishing anchored directory stage"),
        }
    }

    pub(crate) fn create_symlink(&self, relative: &Path, target: &Path) -> Result<bool> {
        let (parent, leaf) = self.open_parent(relative)?;
        match nix::unistd::symlinkat(target, &parent, leaf.as_os_str()) {
            Ok(()) => {
                nix::unistd::fsync(&parent)?;
                Ok(true)
            }
            Err(Errno::EEXIST) => Ok(false),
            Err(error) => Err(error).context("creating anchored seed link"),
        }
    }

    pub(crate) fn read_link(&self, relative: &Path) -> Result<Option<PathBuf>> {
        let (parent, leaf) = self.open_parent(relative)?;
        match nix::fcntl::readlinkat(&parent, leaf.as_os_str()) {
            Ok(target) => Ok(Some(target.into())),
            Err(Errno::ENOENT | Errno::EINVAL) => Ok(None),
            Err(error) => Err(error).context("reading anchored link"),
        }
    }

    /// Remove only a caller-owned staging tree, without following its links.
    pub(crate) fn remove_staged_dir(&self, relative: &Path) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative)?;
        let child = match self.child(relative) {
            Ok(child) => child,
            Err(error) if missing_or_hostile(&error) => return self.remove_file(relative),
            Err(error) => return Err(error),
        };
        child.remove_contents()?;
        unlinkat(&parent, leaf.as_os_str(), UnlinkatFlags::RemoveDir)
            .context("removing anchored staging directory")
    }

    /// Empty the pinned directory, never reopening its replaceable root path.
    pub(crate) fn remove_contents(&self) -> Result<()> {
        for name in self.read_dir(Path::new(""), usize::MAX)? {
            let path = Path::new(&name);
            match self.child(path) {
                Ok(child) => {
                    child.remove_contents()?;
                    unlinkat(&self.fd, name.as_os_str(), UnlinkatFlags::RemoveDir)
                        .context("removing emptied anchored child")?;
                }
                Err(error) if missing_or_hostile(&error) => self.remove_file(path)?,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn modified(&self, relative: &Path, directory: bool) -> Result<Option<std::time::SystemTime>> {
        let fd = if directory {
            match self.open_dir(relative) {
                Ok(fd) => fd,
                Err(error) if missing_or_hostile(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        } else {
            let Some(file) = self.open_regular(relative, usize::MAX)? else {
                return Ok(None);
            };
            file.into()
        };
        let stat = fstat(&fd)?;
        let seconds = u64::try_from(stat.st_mtime).unwrap_or(0);
        let nanos = u32::try_from(stat.st_mtime_nsec)
            .unwrap_or(0)
            .min(999_999_999);
        Ok(Some(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(seconds, nanos),
        ))
    }

    fn open_dir(&self, relative: &Path) -> Result<OwnedFd> {
        let mut current = openat(
            &self.fd,
            ".",
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        for component in normal_components(relative)? {
            current = openat(
                &current,
                component.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )
            .context("opening anchored directory component")?;
        }
        Ok(current)
    }

    fn open_parent(&self, relative: &Path) -> Result<(OwnedFd, std::ffi::OsString)> {
        let mut components = normal_components(relative)?;
        let Some(leaf) = components.pop() else {
            bail!("anchored file path has no leaf");
        };
        let mut current = openat(
            &self.fd,
            ".",
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        for component in components {
            current = openat(
                &current,
                component.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )
            .context("opening anchored parent component")?;
        }
        Ok((current, leaf))
    }
}

fn missing_or_hostile(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Errno>()
        .is_some_and(|errno| matches!(*errno, Errno::ENOENT | Errno::ELOOP | Errno::ENOTDIR))
}

fn normal_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => result.push(value.to_os_string()),
            _ => bail!("anchored path contains a non-normal component"),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_directory_sync_failure_is_retryable() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FAIL_SYNC_ONCE.set(None);
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let root = AnchoredDir::open(temporary.path()).unwrap();
        std::fs::write(temporary.path().join("original"), b"UNCHANGED").unwrap();
        let _reset = Reset;
        for _ in 0..2 {
            FAIL_SYNC_ONCE.set(Some(temporary.path().join("tree")));
            assert!(root.create_child(Path::new("tree/nested/leaf")).is_err());
            assert!(!temporary.path().join("tree/nested/leaf").exists());
            assert_eq!(
                std::fs::read(temporary.path().join("original")).unwrap(),
                b"UNCHANGED"
            );
        }
        FAIL_SYNC_ONCE.set(None);
        let directory = root.create_child(Path::new("tree/nested/leaf")).unwrap();
        assert_eq!(
            directory.identity().unwrap(),
            root.child(Path::new("tree/nested/leaf"))
                .unwrap()
                .identity()
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestors_open_but_a_symlinked_anchor_leaf_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(real.join("store")).unwrap();
        std::fs::write(real.join("store/id"), b"anchored").unwrap();
        std::os::unix::fs::symlink(&real, temp.path().join("link")).unwrap();
        for (anchor, id) in [("link/store", Some(&b"anchored"[..])), ("link", None)] {
            let opened = AnchoredDir::open(&temp.path().join(anchor))
                .ok()
                .map(|dir| dir.read_regular(Path::new("id"), 64).unwrap());
            assert_eq!(
                opened.as_ref().map(|read| read.as_deref()),
                id.map(Some),
                "{anchor}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_components_and_bounds_reads() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let anchored = AnchoredDir::open(&root).unwrap();
        anchored.ensure_dir(Path::new("safe")).unwrap();
        std::fs::write(root.join("safe/id"), b"abcd").unwrap();
        assert_eq!(
            anchored.read_regular(Path::new("safe/id"), 4).unwrap(),
            Some(b"abcd".to_vec())
        );
        assert_eq!(
            anchored.read_regular(Path::new("safe/id"), 3).unwrap(),
            None
        );

        symlink(outside.path(), root.join("escape")).unwrap();
        symlink(root.join("safe/id"), root.join("linked-id")).unwrap();
        let pipe = root.join("pipe");
        nix::unistd::mkfifo(&pipe, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        use std::os::unix::fs::OpenOptionsExt;
        let pipe_guard = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&pipe)
            .unwrap();
        assert!(anchored.read_regular(Path::new("escape/id"), 8).is_err());
        assert_eq!(
            anchored.read_regular(Path::new("linked-id"), 8).unwrap(),
            None
        );
        assert_eq!(anchored.read_regular(Path::new("pipe"), 8).unwrap(), None);
        drop(pipe_guard);
        assert!(anchored.ensure_dir(Path::new("escape/child")).is_err());
        assert!(!outside.path().join("child").exists());
    }

    #[test]
    fn rejected_file_stage_is_never_visible_in_the_active_root() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("active");
        let private_path = directory.path().join("private");
        std::fs::create_dir(&active_path).unwrap();
        std::fs::create_dir(&private_path).unwrap();
        std::fs::write(active_path.join("config"), b"LOCAL_CONFIG").unwrap();
        let active = AnchoredDir::open(&active_path).unwrap();
        let private = AnchoredDir::open(&private_path).unwrap();
        let reject = || {
            let entries = std::fs::read_dir(&active_path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            assert_eq!(entries, vec![std::ffi::OsString::from("config")]);
            assert_eq!(
                std::fs::read(active_path.join("config")).unwrap(),
                b"LOCAL_CONFIG"
            );
            bail!("source changed before publication")
        };
        assert!(active
            .publish_file(
                Path::new("config"),
                &mut &b"FOREIGN_CONTEXT"[..],
                std::fs::Permissions::from_mode(0o600),
                true,
                Some(FilePublication {
                    staging: &private,
                    validate: &reject
                })
            )
            .is_err());
        assert_eq!(
            std::fs::read(active_path.join("config")).unwrap(),
            b"LOCAL_CONFIG"
        );
        let accept = || Ok(());
        active
            .publish_file(
                Path::new("config"),
                &mut &b"APPROVED_CONFIG"[..],
                std::fs::Permissions::from_mode(0o600),
                true,
                Some(FilePublication {
                    staging: &private,
                    validate: &accept,
                }),
            )
            .unwrap();
        assert_eq!(
            std::fs::read(active_path.join("config")).unwrap(),
            b"APPROVED_CONFIG"
        );
    }
}

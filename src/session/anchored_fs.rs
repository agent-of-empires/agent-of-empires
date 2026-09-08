//! File operations rooted at a directory descriptor.

use anyhow::{bail, Context, Result};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{open, openat, AtFlags, OFlag};
use nix::sys::stat::{fstat, fstatat, mkdirat, Mode};
use nix::unistd::{unlinkat, UnlinkatFlags};
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

impl AnchoredDir {
    /// Anchor at `path`, whose ancestors are resolved the way any other
    /// caller resolves them and whose own leaf may not be a symlink.
    ///
    /// Walking the ancestors with `O_NOFOLLOW` defended nothing: a hostile
    /// `/var` is not a threat this type can answer, while macOS reaches both
    /// `/tmp` and the per-user temp root through a symlink, so the walk
    /// refused every anchored read on that platform. The leaf keeps
    /// `O_NOFOLLOW` because it is the swap an attacker controls, and so does
    /// every component below it, which is the escape this type exists to stop.
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
        let components = normal_components(relative)?;
        let mut current = openat(
            &self.fd,
            ".",
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        for component in &components {
            match mkdirat(&current, component.as_os_str(), Mode::S_IRWXU) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(error) => return Err(error).context("creating anchored directory"),
            }
            current = openat(
                &current,
                component.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
                Mode::empty(),
            )
            .context("opening anchored directory component")?;
        }
        Ok(self.root.join(relative))
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

    pub(crate) fn regular_exists(&self, relative: &Path) -> bool {
        self.open_parent(relative)
            .ok()
            .and_then(|(parent, leaf)| {
                fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).ok()
            })
            .is_some_and(|stat| (stat.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFREG)
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative)?;
        match unlinkat(&parent, leaf.as_os_str(), UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(error) => Err(error).context("removing anchored file"),
        }
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

    /// A symlinked ancestor of the anchor is normal on macOS, where `/tmp`
    /// and the per-user temp root under `/var` both resolve through one, and
    /// it says nothing about whether the store below the anchor is safe.
    /// Refusing it made every anchored read fail there.
    #[cfg(unix)]
    #[test]
    fn opens_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(real.join("store")).unwrap();
        std::fs::write(real.join("store/id"), b"anchored").unwrap();
        symlink(&real, temp.path().join("via-link")).unwrap();

        let anchored = AnchoredDir::open(&temp.path().join("via-link/store"))
            .expect("an anchor reached through a symlinked ancestor must open");
        assert_eq!(
            anchored
                .read_regular(Path::new("id"), 64)
                .unwrap()
                .as_deref(),
            Some(&b"anchored"[..])
        );
    }

    /// The anchor's own leaf still may not be a symlink: that is the swap an
    /// attacker controls, unlike the system directories above it.
    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_anchor_leaf() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, temp.path().join("leaf-link")).unwrap();

        assert!(AnchoredDir::open(&temp.path().join("leaf-link")).is_err());
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
}

//! File operations rooted at a directory descriptor.

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, openat, AtFlags, OFlag};
use nix::sys::stat::{fstat, fstatat, mkdirat, Mode};
use nix::unistd::{unlinkat, UnlinkatFlags};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

pub(crate) struct AnchoredDir {
    root: PathBuf,
    fd: OwnedFd,
}

impl AnchoredDir {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspecting anchored directory {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "anchored root is a symlink or non-directory: {}",
                path.display()
            );
        }
        let root = path.to_path_buf();
        let fd = open(
            path,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )
        .with_context(|| format!("opening anchored directory {}", root.display()))?;
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

    pub(crate) fn read_regular(
        &self,
        relative: &Path,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
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
        if (stat.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFREG {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(max_bytes.min(4096));
        File::from(fd)
            .take(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Ok(None);
        }
        Ok(Some(bytes))
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
    #[cfg(unix)]
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
        assert!(anchored.read_regular(Path::new("escape/id"), 8).is_err());
        assert!(anchored.ensure_dir(Path::new("escape/child")).is_err());
        assert!(!outside.path().join("child").exists());
    }
}

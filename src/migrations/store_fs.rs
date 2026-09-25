//! Filesystem primitives for the v027 sandbox store move.
//!
//! The move stages a tree, makes it durable, then publishes it with one
//! rename. Two costs dominated that: it wrote every byte of every file, and it
//! forced a full drive cache flush per file. Both have cheaper answers that
//! keep the guarantee the publish actually needs, which is that the staged
//! tree is on the media before the rename is.

use std::fs;
use std::io;
use std::path::Path;

/// Whether this move may still try a copy-on-write clone.
///
/// A filesystem pair answers the same way for every file in one tree, so the
/// first refusal turns clones off for the rest of the move instead of paying
/// a failing syscall per file.
#[cfg(unix)]
#[derive(Default)]
pub(super) struct CloneSupport {
    refused: bool,
}

#[cfg(unix)]
impl CloneSupport {
    /// Create `target` as a copy-on-write clone of the open regular file
    /// `source`, described by `stat`. `target` must not exist.
    ///
    /// `None` means the caller must copy the bytes itself; nothing is left at
    /// `target`. Every failure answers that way, not only a filesystem's
    /// refusal: `clonefile` fails across filesystems and on non-APFS volumes,
    /// `FICLONE` fails outside btrfs and XFS, and a store move that aborted on
    /// any of those would be worse than one that copies.
    pub(super) fn clone_file(
        &mut self,
        source: &fs::File,
        stat: &libc::stat,
        target: &Path,
    ) -> Option<fs::File> {
        if self.refused {
            return None;
        }
        match clone_regular_file(source, stat, target) {
            Ok(file) => Some(file),
            Err(error) => {
                tracing::debug!(
                    "v027 copying {} rather than cloning it: {error}",
                    target.display()
                );
                self.refused = true;
                None
            }
        }
    }
}

/// `clonefile` copies content, mode, timestamps and extended attributes, so
/// the clone needs nothing stamped onto it afterwards. The returned handle is
/// read-only: writing through it would break the sharing the clone exists for.
#[cfg(target_vendor = "apple")]
fn clone_regular_file(
    source: &fs::File,
    _stat: &libc::stat,
    target: &Path,
) -> io::Result<fs::File> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let name = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `source` is an open descriptor for the call's duration and
    // `name` is a NUL-terminated path valid for the same span.
    let cloned =
        unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, name.as_ptr(), 0) };
    if cloned != 0 {
        return Err(io::Error::last_os_error());
    }
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
}

/// `FICLONE` shares the source's extents and nothing else, so the clone is
/// stamped with the source's mode and timestamps here, exactly as the copy
/// path stamps a file it wrote.
#[cfg(target_os = "linux")]
fn clone_regular_file(source: &fs::File, stat: &libc::stat, target: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;

    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)?;
    // SAFETY: both descriptors are open for the call's duration and `FICLONE`
    // takes the source descriptor by value, not a pointer.
    let cloned = unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if cloned != 0 {
        let error = io::Error::last_os_error();
        drop(output);
        let _ = fs::remove_file(target);
        return Err(error);
    }
    output.set_permissions(fs::Permissions::from_mode(stat.st_mode))?;
    nix::sys::stat::futimens(
        &output,
        &nix::sys::time::TimeSpec::new(stat.st_atime, stat.st_atime_nsec),
        &nix::sys::time::TimeSpec::new(stat.st_mtime, stat.st_mtime_nsec),
    )?;
    Ok(output)
}

/// Unix without a reflink call of its own copies, as it always has.
#[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "linux")))]
fn clone_regular_file(
    _source: &fs::File,
    _stat: &libc::stat,
    _target: &Path,
) -> io::Result<fs::File> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// `fsync(2)` on a file or directory the move just created.
///
/// Deliberately not [`fs::File::sync_all`]: on Apple platforms the standard
/// library maps both `sync_all` and `sync_data` to `F_FULLFSYNC`, a whole
/// drive cache flush that measured 7.29 ms per file against 0.58 ms for plain
/// `fsync` and made the flushing, not the copying, the whole runtime of the
/// move (#3819). Per entry the move needs only that the bytes reach the
/// drive; [`barrier`] is what orders them ahead of the publish.
pub(super) fn sync_to_drive(file: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `file` owns an open descriptor for the call's duration.
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    file.sync_all()
}

/// Order everything [`sync_to_drive`] has pushed ahead of the rename that
/// publishes the staged tree. `dir` is any directory on the volume the
/// publish happens on.
///
/// On Apple platforms `fsync` moves a file's bytes to the drive but not onto
/// the media, and the drive may commit what is still in its cache out of
/// order; `F_BARRIERFSYNC` costs one call per publish and forbids that
/// reordering across the barrier, so no crash can expose the rename without
/// the data it published. `F_FULLFSYNC` is the fallback for a volume that
/// refuses the barrier: stronger, and still one call rather than one per file.
/// Elsewhere `fsync` already reaches the media, so the per-entry calls have
/// left nothing to order.
pub(super) fn barrier(dir: &fs::File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::io::AsRawFd;
        for command in [libc::F_BARRIERFSYNC, libc::F_FULLFSYNC] {
            // SAFETY: `dir` owns an open descriptor for the call's duration
            // and neither command reads an argument.
            if unsafe { libc::fcntl(dir.as_raw_fd(), command) } != -1 {
                return Ok(());
            }
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A clone the filesystem cannot serve must read as "copy it yourself",
    /// never as a failed migration. A character device is a source no clone
    /// implementation accepts, so this holds on every platform, including one
    /// whose filesystem would happily clone a regular file.
    #[test]
    fn an_unclonable_source_falls_back_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("copy");
        let source = fs::File::open("/dev/null").unwrap();
        let stat = nix::sys::stat::fstat(&source).unwrap();
        let mut support = CloneSupport::default();

        assert!(support.clone_file(&source, &stat, &target).is_none());
        assert!(
            !target.exists(),
            "a refused clone must not leave a partial file behind"
        );
        assert!(
            support.refused,
            "one refusal must stop the move retrying a clone per file"
        );
    }

    /// Once refused, later files skip the syscall entirely rather than paying
    /// a failure each.
    #[test]
    fn a_refused_clone_is_not_retried() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        fs::write(&source_path, b"contents").unwrap();
        let source = fs::File::open(&source_path).unwrap();
        let stat = nix::sys::stat::fstat(&source).unwrap();
        let mut support = CloneSupport { refused: true };

        assert!(support
            .clone_file(&source, &stat, &temp.path().join("copy"))
            .is_none());
        assert!(!temp.path().join("copy").exists());
    }

    #[test]
    fn syncing_and_barriering_a_directory_succeed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"contents").unwrap();
        let file = fs::File::open(temp.path().join("file")).unwrap();
        let dir = fs::File::open(temp.path()).unwrap();

        sync_to_drive(&file).unwrap();
        sync_to_drive(&dir).unwrap();
        barrier(&dir).unwrap();
    }
}

//! Owner-only persistence shared by the login-session and passkey stores.

use std::path::Path;

/// Fail-closed check that `path` is safe to read from or write to: no symlinks, no
/// group/world-writable parent, no group/world-accessible file.
pub(super) fn check_path_security(path: &Path) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    if let Some(parent) = path.parent() {
        let pmeta = std::fs::symlink_metadata(parent).context("stat store parent dir")?;
        if pmeta.file_type().is_symlink() {
            bail!("store parent dir is a symlink; refusing");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if pmeta.permissions().mode() & 0o022 != 0 {
                bail!("store parent dir is group/world writable; refusing");
            }
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("store path is a symlink; refusing");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    bail!("store file is group/world accessible; refusing");
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("stat store file"),
    }
}

/// Atomically write `bytes` to `path` after `check_path_security`, leaving it 0600.
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    check_path_security(path)?;
    crate::session::atomic_write(path, bytes)?;
    // `atomic_write` lands the file via a `NamedTempFile` (0600 on first create); re-assert
    // it so an earlier file with looser perms can never stay widened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

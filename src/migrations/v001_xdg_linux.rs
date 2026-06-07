//! Migration v001: Move to XDG Base Directory on Linux
//!
//! Previously: ~/.hmp/
//! After:     $XDG_CONFIG_HOME/hmp/ (defaults to ~/.config/hmp/)
//!
//! This migration moves all data from the legacy location to the XDG-compliant location.
//! On non-Linux platforms, this is a no-op.

use anyhow::Result;
use tracing::debug;

#[cfg(target_os = "linux")]
use {std::fs, tracing::info};

pub fn run() -> Result<()> {
    if cfg!(debug_assertions) {
        debug!("Skipping v001 XDG migration in debug build (dev namespace is isolated from release data)");
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
        let legacy_dir = home.join(".hmp");

        let config_dir =
            dirs::config_dir().ok_or_else(|| anyhow::anyhow!("Cannot find config directory"))?;
        let xdg_dir = config_dir.join("hmp");

        if !legacy_dir.exists() {
            debug!("No legacy directory found, skipping XDG migration");
            return Ok(());
        }

        if xdg_dir.exists() {
            debug!("XDG directory already exists, skipping migration");
            return Ok(());
        }

        info!(
            "Migrating data from {} to {}",
            legacy_dir.display(),
            xdg_dir.display()
        );

        fs::create_dir_all(&xdg_dir)?;

        for entry in fs::read_dir(&legacy_dir)? {
            let entry = entry?;
            let source = entry.path();
            let dest = xdg_dir.join(entry.file_name());

            if source.is_dir() {
                copy_dir_recursive(&source, &dest)?;
            } else {
                fs::copy(&source, &dest)?;
            }
        }

        fs::remove_dir_all(&legacy_dir)?;
        info!("Migration complete");
    }

    #[cfg(not(target_os = "linux"))]
    {
        debug!("XDG migration only applies to Linux, skipping");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

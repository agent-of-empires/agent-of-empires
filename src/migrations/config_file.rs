//! Shared config.toml enumeration and rewriting for migrations.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

/// The global `config.toml` plus every `profiles/*/config.toml`.
pub(super) fn all_configs(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![app_dir.join("config.toml")];
    paths.extend(profile_configs(app_dir)?);
    Ok(paths)
}

/// Every `profiles/*/config.toml`.
pub(super) fn profile_configs(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let profiles = app_dir.join("profiles");
    if !profiles.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&profiles)? {
        let path = entry?.path();
        if path.is_dir() {
            paths.push(path.join("config.toml"));
        }
    }
    Ok(paths)
}

/// Hand the parsed contents of `path` to `edit`, writing them back only when
/// it reports a change. A missing file is skipped, and so is one that does not
/// parse: a migration correcting a value must not abort boot over a config the
/// user can still fix by hand.
pub(super) fn rewrite(path: &Path, edit: impl FnOnce(&mut toml::Table) -> bool) -> Result<()> {
    rewrite_inner(path, edit, None)
}

/// [`rewrite`] for a migration that carries a user's value forward, where a
/// parse error propagates instead: the schema bump would otherwise drop that
/// value for good, since the legacy key is never read again.
pub(super) fn rewrite_strict(
    path: &Path,
    migration: &str,
    edit: impl FnOnce(&mut toml::Table) -> bool,
) -> Result<()> {
    rewrite_inner(path, edit, Some(migration))
}

/// `strict` names the migration whose parse error propagates; `None` skips.
fn rewrite_inner(
    path: &Path,
    edit: impl FnOnce(&mut toml::Table) -> bool,
    strict: Option<&str>,
) -> Result<()> {
    if !path.exists() {
        debug!("config {} does not exist, skipping", path.display());
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = match (content.parse(), strict) {
        (Ok(table), _) => table,
        (Err(e), Some(migration)) => {
            return Err(e).with_context(|| {
                format!(
                    "Failed to parse {} during {migration} migration",
                    path.display()
                )
            })
        }
        (Err(e), None) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };
    if !edit(&mut doc) {
        return Ok(());
    }
    crate::session::atomic_write(path, toml::to_string_pretty(&doc)?.as_bytes())?;
    Ok(())
}

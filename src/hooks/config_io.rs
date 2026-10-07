//! Locked, symlink-aware reads and writes of agent config files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde_json::{Map, Value};

/// How a config write treats a symlink on its path.
///
/// `Follow` is for the user's own host config, where a dotfiles link must
/// survive (#2784, #3186). `Never` is for a directory bind-mounted into a
/// container, where a link is an attempt to redirect the write onto a host file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymlinkPolicy {
    Follow,
    Never,
}

impl SymlinkPolicy {
    /// The content to merge into, or `None` when absent. Under `Never` anything
    /// but a regular file reads as absent, checked and read on one `O_NOFOLLOW` fd.
    pub(crate) fn read(self, path: &Path) -> Result<Option<String>> {
        match self {
            Self::Follow => match std::fs::read_to_string(path) {
                Ok(content) => Ok(Some(content)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            },
            Self::Never => {
                let (dir, name) = split_in_bind(path)?;
                crate::session::read_file_no_follow(dir, Path::new(name))
            }
        }
    }

    pub(super) fn write(self, path: &Path, content: &[u8]) -> Result<()> {
        match self {
            Self::Follow => write_file(path, content),
            Self::Never => {
                let (dir, name) = split_in_bind(path)?;
                crate::session::replace_file_no_follow(dir, Path::new(name), content)
            }
        }
    }

    /// Resolving the chain keeps two writers to one dotfile target on one lock;
    /// under `Never` resolving would move the lock outside the bind.
    pub(super) fn lock_path(self, path: &Path) -> Result<PathBuf> {
        match self {
            Self::Follow => crate::session::resolve_symlink_chain(path),
            Self::Never => Ok(path.to_path_buf()),
        }
    }
}

/// The bind root is the directory AoE owns and the file sits directly in it.
fn split_in_bind(path: &Path) -> Result<(&Path, &std::ffi::OsStr)> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    Ok((dir, name))
}

/// Creates the parent directory, then atomically replaces the file.
pub(super) fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::session::atomic_write(path, content)
}

pub(super) fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_file(path, serde_json::to_string_pretty(value)?.as_bytes())
}

/// Parses a JSON config, treating malformed content as empty so a torn file
/// cannot block a launch.
pub(super) fn parse_json_or_empty(content: Option<String>, path: &Path) -> Value {
    let Some(content) = content else {
        return serde_json::json!({});
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", path.display(), e);
        serde_json::json!({})
    })
}

/// The object at `key`, replacing a missing or non-object value.
pub(super) fn object_at<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> &'a mut Map<String, Value> {
    let value = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("ensured object above")
}

pub(super) fn log_unchanged(path: &Path) {
    tracing::debug!(target: "hooks.install",
        "AoE hooks in {} already up to date; skipping write", path.display());
}

pub(super) fn log_installed(path: &Path) {
    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", path.display());
}

pub(super) fn log_removed(path: &Path) {
    tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", path.display());
}

pub(super) fn with_config_lock<T>(
    path: &Path,
    lock_extension: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_config_lock_policy(path, lock_extension, SymlinkPolicy::Follow, f)
}

/// Holds an exclusive `flock` on `<path>.<lock_extension>` while `f` runs, so
/// concurrent installers cannot interleave a stale read with a write. Under
/// `Never` the lock file is opened `O_NOFOLLOW`, so a planted link fails the write.
pub(crate) fn with_config_lock_policy<T>(
    path: &Path,
    lock_extension: &str,
    policy: SymlinkPolicy,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension(lock_extension);
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600);
    if policy == SymlinkPolicy::Never {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock_file = options
        .open(&lock_path)
        .with_context(|| format!("Failed to open config lock {}", lock_path.display()))?;

    lock_file
        .lock_exclusive()
        .with_context(|| format!("Failed to lock config {}", path.display()))?;

    let result = f();
    let unlock_result = fs2::FileExt::unlock(&lock_file);
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => {
            Err(error).with_context(|| format!("Failed to unlock {}", lock_path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_released_when_the_closure_panics() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("panicky.json");
        let panicked = std::panic::catch_unwind(|| {
            with_config_lock(&target, "json.lock", || -> Result<()> { panic!("boom") })
        });
        assert!(panicked.is_err());
        let started = std::time::Instant::now();
        with_config_lock(&target, "json.lock", || Ok(())).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }
}

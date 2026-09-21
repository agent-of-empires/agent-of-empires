//! Shared hook test helpers. Hook-base tests must also run under
//! `serial_test::serial(hook_base)`, since the base override is thread-local.

#![cfg(test)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Installs a hook-base override, cleared on drop.
pub(crate) struct BaseGuard;

impl BaseGuard {
    /// Base under a fresh tempdir, not yet created on disk.
    pub(crate) fn fresh() -> (Self, PathBuf, TempDir) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks");
        super::dir_guard::override_base_for_test(base.clone());
        super::dir_guard::reset_for_test();
        (Self, base, tmp)
    }

    /// As [`Self::fresh`], with the base created at 0o700.
    pub(crate) fn ready() -> (Self, PathBuf, TempDir) {
        let (g, base, tmp) = Self::fresh();
        make_correct_base(&base);
        (g, base, tmp)
    }

    /// An explicit base; the caller creates any parents it needs.
    pub(crate) fn with_base(base: PathBuf) -> Self {
        super::dir_guard::override_base_for_test(base);
        super::dir_guard::reset_for_test();
        Self
    }
}

impl Drop for BaseGuard {
    fn drop(&mut self) {
        super::dir_guard::clear_base_override_for_test();
        super::dir_guard::reset_for_test();
    }
}

pub(crate) fn make_correct_base(p: &Path) {
    std::fs::create_dir(p).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
pub(crate) fn privdrop_test_enabled() -> bool {
    if std::env::var("AOE_PRIVDROP_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set AOE_PRIVDROP_TESTS=1 in the dedicated Linux job");
        return false;
    }
    assert!(
        nix::unistd::geteuid().is_root(),
        "AOE_PRIVDROP_TESTS requires the prebuilt test binary to run as root"
    );
    true
}

#[cfg(target_os = "linux")]
pub(crate) fn make_alien_owned(path: &Path) -> u32 {
    use nix::unistd::{chown, Gid, Uid};

    assert!(
        nix::unistd::geteuid().is_root(),
        "alien ownership fixtures require root"
    );
    let alien_uid = Uid::from_raw(65_534);
    let alien_gid = Gid::from_raw(65_534);
    chown(path, Some(alien_uid), Some(alien_gid))
        .unwrap_or_else(|e| panic!("chown {} to nobody: {e}", path.display()));
    let metadata = std::fs::symlink_metadata(path).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        metadata.uid(),
        alien_uid.as_raw(),
        "fixture owner did not change"
    );
    alien_uid.as_raw()
}

/// Resolved default hook events for `agent`, with `overrides` applied to its status map.
pub(crate) fn agent_events(
    agent: &str,
    overrides: &[(&str, crate::agents::HookStatus)],
) -> Vec<crate::agents::ResolvedHookEvent> {
    let mut config = crate::session::config::Config::default();
    for (event, status) in overrides {
        config
            .agents
            .entry(agent.to_string())
            .or_default()
            .status_map
            .insert(event.to_string(), *status);
    }
    let agent = crate::agents::get_agent(agent).unwrap();
    if agent.hook_config.is_some() {
        crate::agents::resolved_hook_events(agent, &config).unwrap()
    } else {
        crate::agents::resolved_sidecar_hook_events(agent, &config).unwrap()
    }
}

pub(crate) fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Runs `f` and asserts `path` kept its bytes, inode and mtime.
pub(crate) fn assert_not_rewritten(path: &Path, f: impl FnOnce()) {
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();
    let meta = file.metadata().unwrap();
    let bytes = std::fs::read(path).unwrap();
    f();
    let after = std::fs::metadata(path).unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "{} bytes changed",
        path.display()
    );
    assert_eq!(after.ino(), meta.ino(), "{} replaced", path.display());
    assert_eq!(
        after.modified().unwrap(),
        meta.modified().unwrap(),
        "{} rewritten",
        path.display()
    );
}

//! Shared test helpers for hook-base scaffolding.
//!
//! Used by `dir_guard`, `status_file`, `cli::extract_session_id`, and
//! `session::container_config` tests so the override-and-reset dance lives
//! in one place. Tests using these helpers MUST also gate via
//! `serial_test::serial(hook_base)` so the thread-local override stays
//! consistent across parallel runs.

#![cfg(test)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// RAII guard that installs a per-test hook-base override on construction
/// and clears it on drop.
pub(crate) struct BaseGuard;

impl BaseGuard {
    /// Tempdir created, override registered, base path NOT created on disk.
    /// Use for tests that exercise mkdir semantics themselves (mode/owner
    /// rejection, symlink rejection, fresh-init behavior).
    pub(crate) fn fresh() -> (Self, PathBuf, TempDir) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks");
        super::dir_guard::override_base_for_test(base.clone());
        super::dir_guard::reset_for_test();
        (Self, base, tmp)
    }

    /// As [`Self::fresh`] but with the base already created at 0o700. Use for
    /// tests that just need a working hook base in place.
    pub(crate) fn ready() -> (Self, PathBuf, TempDir) {
        let (g, base, tmp) = Self::fresh();
        make_correct_base(&base);
        (g, base, tmp)
    }
}

impl Drop for BaseGuard {
    fn drop(&mut self) {
        super::dir_guard::clear_base_override_for_test();
        super::dir_guard::reset_for_test();
    }
}

/// Create the hook base directory at `p` with mode 0o700. Companion to
/// [`BaseGuard::fresh`] for tests that want to verify mkdir semantics
/// then continue with a normally-shaped base.
pub(crate) fn make_correct_base(p: &Path) {
    std::fs::create_dir(p).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
}

//! RAII `HOME`/`XDG_CONFIG_HOME` test isolation with restore-on-drop.
//!
//! Distinct from `crate::common::setup_temp_home`, which overrides the
//! env vars without restoring the previous values. It is also a separate
//! copy from `src/session/test_support.rs` (used by unit tests compiled
//! into the library) and `tests/e2e/harness.rs` (used by the `tests/e2e`
//! binary); those are different crates and cannot share this code.

use std::path::Path;

/// RAII guard: points `HOME`/`XDG_CONFIG_HOME` at `temp` for the test
/// body and restores the prior values on `Drop`. `#[serial]` on every
/// caller linearizes this against other tests in the binary; without
/// the restore, a later test could inherit this test's (by-then-dropped)
/// tempdir path.
#[must_use = "HomeGuard restores env vars on Drop; bind it, don't discard it, or isolation ends immediately"]
pub struct HomeGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
}

impl HomeGuard {
    /// Snapshots the current `HOME`/`XDG_CONFIG_HOME` before overriding them,
    /// so `Drop` can restore the caller's real environment.
    pub fn new(temp: &Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: env mutation; #[serial] linearizes this against every
        // other #[serial] test in the binary, so no concurrent
        // reader/writer exists.
        unsafe { std::env::set_var("HOME", temp) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", temp.join(".config")) };
        Self {
            prev_home,
            prev_xdg,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        /// Restores `key` to its prior value, or removes it if it was
        /// previously unset.
        fn restore_or_remove(key: &str, prev: Option<std::ffi::OsString>) {
            // SAFETY: same invariant as HomeGuard::new; #[serial] guards this.
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        restore_or_remove("HOME", self.prev_home.take());
        restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
    }
}

/// Thin wrapper kept so existing call sites don't need renaming; see
/// `HomeGuard` for the isolation/restore behavior.
pub fn isolate_home(temp: &Path) -> HomeGuard {
    HomeGuard::new(temp)
}

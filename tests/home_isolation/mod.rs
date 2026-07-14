//! Shared `HOME`/`XDG_CONFIG_HOME` test-isolation support for the
//! standalone integration-test binaries under `tests/` that don't also
//! need `tests/common`'s port helpers.
//!
//! This is a separate module (not folded into `tests/common/mod.rs`)
//! because binaries that include a module get `dead_code` warnings for
//! any of its public items they don't call — `pick_free_port`/
//! `wait_for_port` in `tests/common/mod.rs` are unused by the isolation-only
//! consumers below, so this splits the two concerns into separate shared
//! modules. It is also a separate copy from `src/session/test_support.rs`
//! (the in-crate copy, used by unit tests compiled into the library) and
//! from `tests/e2e/harness.rs` (used by the `tests/e2e` binary) — each
//! standalone `tests/*.rs` file Cargo auto-discovers is compiled as its
//! own separate crate, and `tests/e2e` is yet another separate crate, so
//! none of the three can share code directly across those boundaries.
//!
//! Naming this file `mod.rs` (rather than a sibling `tests/home_isolation.rs`)
//! opts it out of Cargo's test-binary auto-discovery, so it stays a
//! shared module rather than becoming its own (empty) test binary.

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

//! Shared test-support helpers.
//!
//! Tests across the crate need to point `HOME` (and, on Linux/macOS,
//! `XDG_CONFIG_HOME` plus `XDG_DATA_HOME`) at an isolated temporary
//! directory so both the crate's dot-dir resolution and the opencode
//! data-dir lookup land in a scratch location instead of the caller's
//! real dirs. Returning a bare `TempDir` from the old duplicated
//! `isolate_app_dir` helpers meant the tempdir was cleaned up on drop,
//! but the env vars leaked into the process for the rest of the test
//! run, poisoning any later test that read them. See issue #2306.
//!
//! `AppDirGuard` snapshots the previous env values, so its `Drop`
//! closes the leak: env vars are restored to their prior state (or
//! removed if they were previously unset) even if the test panics.
//! The `Drop` restore-or-remove pattern is similar to
//! `StorageHomeGuard` at [`crate::session::sync`], with the deliberate
//! divergence that `AppDirGuard` snapshots `OsString` via `env::var_os`
//! rather than `String` via `env::var`, so a non-UTF-8 prior value
//! round-trips faithfully instead of coercing to `None`.
//!
//! Two constructors are offered. [`isolate_app_dir`] creates and owns
//! a fresh tempdir; reach for it when the caller has no directory to
//! share. [`isolate_app_dir_at`] takes a caller-owned
//! [`std::path::Path`] and leaves tempdir lifetime management to the
//! caller; reach for it when a struct or helper already owns a
//! `TempDir` and needs the guard to co-exist with it. When the guard
//! does not own the tempdir, the caller must declare the `TempDir`
//! AFTER the guard in the enclosing struct so field drop order runs
//! env-restore before tempdir cleanup.
//!
//! Scope: the guard isolates tests on Linux and macOS (the crate's
//! supported native targets; Windows is WSL2-only per the README). On
//! native Windows `dirs::home_dir()` resolves via
//! `SHGetKnownFolderPath(FOLDERID_Profile)`, not `$HOME`, so a
//! `set_var("HOME", ...)` alone would not redirect
//! `get_app_dir_path`. Isolating tests on native Windows would need a
//! different mechanism (e.g. a stub for the profile-folder lookup)
//! rather than more env vars in this snapshot set.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// RAII guard: isolates `HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME`
/// for one test; restores them on `Drop`. See the
/// [module documentation](crate::session::test_support) for the
/// process-global env-leak motivation (issue #2306).
///
/// `Debug` is intentionally not derived: the snapshot fields carry the
/// caller's real `$HOME`, `$XDG_CONFIG_HOME`, and `$XDG_DATA_HOME`, and
/// a derived `Debug` impl would print them verbatim in test failure
/// output (unwrap-panic
/// backtraces, `assert!` messages that format the guard). Path values on
/// developer machines are often personally identifying; keep the guard
/// opaque.
///
/// The tempdir path itself (returned by [`Self::path`]) is not personally
/// identifying on Linux (`$TMPDIR` defaults to `/tmp`), but on macOS
/// resolves via `_CS_DARWIN_USER_TEMP_DIR` to
/// `/var/folders/xx/yy/T/tmpXXXXXX` where the `xx/yy` fragment is derived
/// from the caller's UID. Call sites should not format `path()` into log
/// output at `info` / `warn` levels for the same reason; use `debug!` or
/// avoid the log entirely.
#[must_use = "AppDirGuard restores env vars on Drop; bind it to `_tmp` or `_guard`, not `_`, or the isolation ends on the same line and the test body runs against the caller's real env"]
pub(crate) struct AppDirGuard {
    // `None` when the caller retains ownership via `isolate_app_dir_at`.
    temp: Option<TempDir>,
    // Snapshotted at construction so `path()` works whether we own the tempdir or not.
    path: PathBuf,
    prev_home: Option<OsString>,
    prev_xdg: Option<OsString>,
    prev_xdg_data: Option<OsString>,
}

impl AppDirGuard {
    /// Returns the app-dir root. See also `impl AsRef<Path>` below: both
    /// accessors are intentionally offered so callers can pick the one
    /// that fits: `guard.path()` for direct use, `&guard` for
    /// `impl AsRef<Path>`-style generic call sites.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Blanket-friendly path access: `app_dir(&guard)` works wherever
/// `impl AsRef<Path>` is accepted. Also enables `&AppDirGuard: AsRef<Path>`
/// via the standard-library blanket `impl<T: AsRef<U>, U> AsRef<U> for &T`.
impl AsRef<Path> for AppDirGuard {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Drop for AppDirGuard {
    fn drop(&mut self) {
        // `prev_xdg` and `prev_xdg_data` are snapshotted unconditionally at
        // construction, while the constructor only mutates `XDG_CONFIG_HOME`
        // and `XDG_DATA_HOME` under `cfg(any(target_os = "linux", target_os
        // = "macos"))`. On other targets the restore writes back the same
        // value the constructor observed (a no-op on the ambient env); the
        // asymmetry is deliberate so a future target that starts consulting
        // these vars inherits the restore path automatically without a
        // matching cfg edit here.
        restore_or_remove("HOME", self.prev_home.take());
        restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
        restore_or_remove("XDG_DATA_HOME", self.prev_xdg_data.take());
        // Env vars are restored; only now release the owned tempdir (if
        // any) so `HOME` no longer points at a directory being deleted.
        let _ = self.temp.take();
    }
}

fn restore_or_remove(key: &str, prev: Option<OsString>) {
    // SAFETY (staged for Rust 2024 edition migration, at which point
    // `std::env::set_var` and `std::env::remove_var` become `unsafe fn`):
    // this function mutates a process-global env slot. It is sound to
    // call as long as no other thread is concurrently reading or writing
    // the same env key. The invariant is enforced by:
    //   1. `AppDirGuard` is only constructed from `#[serial]`-annotated
    //      tests, so the whole call sequence (snapshot -> set_var ->
    //      test body -> Drop -> restore_or_remove) is linearized against
    //      every other `#[serial]` test in the crate.
    //   2. Non-`#[serial]` tests in the crate do not read `HOME`,
    //      `XDG_CONFIG_HOME`, or `XDG_DATA_HOME` (grep-verified; see
    //      the module doc).
    //   3. The `#[tokio::test]` sites that use this helper all run on
    //      the default single-threaded runtime; no worker task reads env
    //      concurrently with the mutation.
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

/// Isolate the app dir for one test using a freshly-created tempdir.
///
/// Points `HOME` at a fresh tempdir root, and on Linux/macOS points
/// `XDG_CONFIG_HOME` at `<tempdir>/.config` and `XDG_DATA_HOME` at
/// `<tempdir>/.local/share` (the shape `get_app_dir_path` and the
/// opencode data-dir lookup both resolve against). The three vars land
/// at different paths on purpose: `HOME` for user-scoped paths,
/// `XDG_CONFIG_HOME` for the crate's own dot-dir subtree, and
/// `XDG_DATA_HOME` for the opencode capture/db subtree.
///
/// The prior values are snapshotted via [`std::env::var_os`] (`OsString`,
/// not `String`) so a non-UTF-8 prior value survives round-tripping through
/// the guard's `Drop`.
///
/// # Panics
///
/// - `TempDir::new()` panics via `.expect(...)` if the OS refuses to
///   create a fresh tempdir (no space on `$TMPDIR`, `EACCES`, filesystem
///   quota). This is the same failure surface as the pre-#2306 helpers
///   this replaced.
/// - `std::env::set_var` panics on a value containing a NUL byte. The
///   value written here is `TempDir::path()`, which cannot contain a NUL
///   on Unix (POSIX pathname rules), so this panic is unreachable in
///   practice.
pub(crate) fn isolate_app_dir() -> AppDirGuard {
    let temp_home = TempDir::new().expect("create tempdir for AppDirGuard");
    let path = temp_home.path().to_path_buf();
    install_env_vars(path, Some(temp_home))
}

/// Isolate the app dir for one test against a caller-owned path.
///
/// Same env-var installation as [`isolate_app_dir`], but the caller owns
/// the tempdir (or any other path they wish to expose as `HOME`). The
/// guard captures neither ownership nor lifetime of `path`; on `Drop`
/// only the env vars are restored.
///
/// The typical shape is:
///
/// ```ignore
/// struct TestEnv {
///     view: HomeView,
///     _guard: crate::session::test_support::AppDirGuard,
///     _temp: tempfile::TempDir,
/// }
/// ```
///
/// Fields drop top-to-bottom, so `view` drops first (any reader of
/// `HOME` runs while the guard is still live), then the guard restores
/// env vars, then `_temp` deletes the tempdir. Declaring `_temp` before
/// `_guard` would delete the dir while `HOME` still points at it.
pub(crate) fn isolate_app_dir_at(path: &Path) -> AppDirGuard {
    install_env_vars(path.to_path_buf(), None)
}

fn install_env_vars(path: PathBuf, temp: Option<TempDir>) -> AppDirGuard {
    let prev_home = std::env::var_os("HOME");
    let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let prev_xdg_data = std::env::var_os("XDG_DATA_HOME");
    // SAFETY (staged for Rust 2024 edition migration): same invariant
    // as [`restore_or_remove`] above. Callers are `#[serial]`-annotated
    // tests; no other thread reads or writes `HOME` / `XDG_CONFIG_HOME`
    // / `XDG_DATA_HOME` while this function runs.
    std::env::set_var("HOME", &path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
        std::env::set_var("XDG_DATA_HOME", path.join(".local/share"));
    }
    AppDirGuard {
        temp,
        path,
        prev_home,
        prev_xdg,
        prev_xdg_data,
    }
}

/// RAII guard: like [`isolate_app_dir`], but for callers that already own
/// their tempdir and just need `HOME`/`XDG_CONFIG_HOME` pointed at an
/// existing path rather than one this helper creates and owns. Sets
/// `XDG_CONFIG_HOME` unconditionally (not gated to Linux/macOS): see
/// issue #1948, the fix this branch lands.
#[must_use = "HomeGuard restores env vars on Drop; bind it to `_home` or `_guard`, not `_`, or the isolation ends on the same line and the test body runs against the caller's real env"]
pub(crate) struct HomeGuard {
    prev_home: Option<OsString>,
    prev_xdg: Option<OsString>,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        restore_or_remove("HOME", self.prev_home.take());
        restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
    }
}

/// Points `HOME`/`XDG_CONFIG_HOME` at `temp` for the current test body,
/// restoring the prior values on `Drop`. Unlike [`isolate_app_dir`], the
/// caller supplies (and owns) the tempdir.
pub(crate) fn isolate_home(temp: &Path) -> HomeGuard {
    let prev_home = std::env::var_os("HOME");
    let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
    // SAFETY (staged for Rust 2024 edition migration): same invariant
    // as [`restore_or_remove`] above.
    std::env::set_var("HOME", temp);
    std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"));
    HomeGuard {
        prev_home,
        prev_xdg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::panic::AssertUnwindSafe;

    /// Locks the fix for #2306: `Drop` MUST restore `HOME` and (on
    /// Linux/macOS) `XDG_CONFIG_HOME` plus `XDG_DATA_HOME` to their
    /// pre-guard values. A future refactor that quietly drops the `Drop`
    /// impl would reintroduce the leak this PR closes.
    #[test]
    #[serial]
    fn app_dir_guard_drop_restores_env_vars() {
        let before_home = std::env::var_os("HOME");
        let before_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let before_xdg_data = std::env::var_os("XDG_DATA_HOME");

        {
            let guard = isolate_app_dir();
            // Byte-identity holds because `TempDir::path()` is
            // un-canonicalized on macOS (`/var/folders/...`, a symlink to
            // `/private/var/folders/...`) and `set_var` writes the bytes
            // verbatim. A future refactor that canonicalizes one side
            // without the other would silently break this on macOS.
            assert_eq!(
                std::env::var_os("HOME"),
                Some(guard.path().as_os_str().to_os_string()),
                "HOME must point at the guard's tempdir during the test body"
            );
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                assert_eq!(
                    std::env::var_os("XDG_CONFIG_HOME"),
                    Some(guard.path().join(".config").into_os_string()),
                    "XDG_CONFIG_HOME must point at <tempdir>/.config"
                );
                assert_eq!(
                    std::env::var_os("XDG_DATA_HOME"),
                    Some(guard.path().join(".local/share").into_os_string()),
                    "XDG_DATA_HOME must point at <tempdir>/.local/share"
                );
            }
        }

        assert_eq!(
            std::env::var_os("HOME"),
            before_home,
            "HOME must be restored on guard Drop"
        );
        assert_eq!(
            std::env::var_os("XDG_CONFIG_HOME"),
            before_xdg,
            "XDG_CONFIG_HOME must be restored on guard Drop"
        );
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME"),
            before_xdg_data,
            "XDG_DATA_HOME must be restored on guard Drop"
        );
    }

    /// Locks the `remove_var` branch of `restore_or_remove`: when the
    /// pre-guard env var was unset, `Drop` MUST leave it unset. Under
    /// Unix CI HOME is always set, so [`app_dir_guard_drop_restores_env_vars`]
    /// above only exercises the `set_var` restoration branch. This test
    /// forces the `None` snapshot by removing both vars before construction.
    ///
    /// The pre-scope removal is wrapped in a small local RAII
    /// (`AmbientEnvRestore`) so a panic in any mid-scope assertion
    /// still restores the caller's original env before the next
    /// `#[serial]` test observes an unset `HOME`.
    #[test]
    #[serial]
    fn app_dir_guard_drop_removes_env_vars_when_unset() {
        struct AmbientEnvRestore {
            home: Option<OsString>,
            xdg: Option<OsString>,
            xdg_data: Option<OsString>,
        }
        impl Drop for AmbientEnvRestore {
            fn drop(&mut self) {
                restore_or_remove("HOME", self.home.take());
                restore_or_remove("XDG_CONFIG_HOME", self.xdg.take());
                restore_or_remove("XDG_DATA_HOME", self.xdg_data.take());
            }
        }

        let _restore = AmbientEnvRestore {
            home: std::env::var_os("HOME"),
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_data: std::env::var_os("XDG_DATA_HOME"),
        };
        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_DATA_HOME");

        {
            let guard = isolate_app_dir();
            // Explicit path check (not just `is_some`) catches a
            // constructor mutation that writes the wrong path when the
            // prior snapshot was `None`.
            assert_eq!(
                std::env::var_os("HOME"),
                Some(guard.path().as_os_str().to_os_string()),
                "constructor must set HOME to the guard tempdir even when the prior value was unset"
            );
        }

        assert_eq!(
            std::env::var_os("HOME"),
            None,
            "HOME must be removed on Drop when it was unset before construction"
        );
        assert_eq!(
            std::env::var_os("XDG_CONFIG_HOME"),
            None,
            "XDG_CONFIG_HOME must stay unset on Drop when it was unset before construction"
        );
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME"),
            None,
            "XDG_DATA_HOME must stay unset on Drop when it was unset before construction"
        );

        // `_restore` fires on scope exit (or on panic before this line):
        // its `Drop` re-applies the ambient env for any downstream
        // serial test.
    }

    /// `AsRef<Path>` lets call sites pass `&guard` wherever a
    /// `Path`-like is expected, matching `Path::join`-style ergonomics.
    #[test]
    #[serial]
    fn app_dir_guard_as_ref_path_matches_path() {
        let guard = isolate_app_dir();
        let via_as_ref: &Path = guard.as_ref();
        assert_eq!(
            via_as_ref,
            guard.path(),
            "AsRef<Path>::as_ref must return the same path as AppDirGuard::path"
        );
    }

    /// Locks the "Drop-runs-on-unwind" contract that motivates the entire
    /// RAII conversion. A future refactor that swaps `AppDirGuard` for a
    /// non-RAII helper (e.g., a `fn cleanup(&self)` the test must call
    /// explicitly) would let a panicking test leak `HOME`/`XDG_CONFIG_HOME`
    /// exactly the way pre-#2306 did.
    #[test]
    #[serial]
    fn app_dir_guard_drop_restores_env_vars_on_panic() {
        let before_home = std::env::var_os("HOME");
        let before_xdg = std::env::var_os("XDG_CONFIG_HOME");

        let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = isolate_app_dir();
            panic!("simulate a test-body panic while the guard is live");
        }));
        assert!(
            unwound.is_err(),
            "the inner panic must actually propagate to catch_unwind"
        );

        assert_eq!(
            std::env::var_os("HOME"),
            before_home,
            "HOME must be restored on guard Drop even when the test body panics"
        );
        assert_eq!(
            std::env::var_os("XDG_CONFIG_HOME"),
            before_xdg,
            "XDG_CONFIG_HOME must be restored on guard Drop even when the test body panics"
        );
    }

    /// Locks the "snapshot at construction" semantic: `Drop` restores to
    /// the pre-construction env values, not to whatever the test last
    /// wrote inside the guard's scope. A refactor that moves the
    /// `env::var_os` snapshot from the constructor into `Drop::drop`
    /// would pass every other test in this module but survive as a
    /// silent regression here.
    #[test]
    #[serial]
    fn app_dir_guard_drop_ignores_mid_scope_env_writes() {
        let before_home = std::env::var_os("HOME");

        {
            let _guard = isolate_app_dir();
            // Mid-scope write that must NOT survive `Drop`. Not the
            // guard's tempdir; a distinct sentinel path. The string
            // shape is Unix-flavoured but is only ever compared as
            // bytes here (never opened as a filesystem path), so
            // `set_var` on any target accepts it without touching the
            // filesystem.
            std::env::set_var("HOME", "/tmp/aoe-mid-scope-sentinel");
            assert_eq!(
                std::env::var_os("HOME"),
                Some(OsString::from("/tmp/aoe-mid-scope-sentinel")),
                "mid-scope write must land while the guard is live"
            );
        }

        assert_eq!(
            std::env::var_os("HOME"),
            before_home,
            "Drop must restore the pre-construction snapshot, not the mid-scope write"
        );
    }

    /// A peer thread writing `HOME` mid-scope must not survive the
    /// guard's `Drop`: `Drop` unconditionally restores the
    /// pre-construction snapshot regardless of intervening writes from
    /// any thread. The barrier rendezvous makes the peer swap land at a
    /// deterministic point in the guard's live scope so this test does
    /// not rely on sampling.
    #[test]
    #[serial]
    fn app_dir_guard_survives_concurrent_peer_env_swap() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let before_home = std::env::var_os("HOME");

        let peer_at_swap = Arc::new(Barrier::new(2));
        let peer_done = Arc::new(Barrier::new(2));

        let peer_at_swap_clone = Arc::clone(&peer_at_swap);
        let peer_done_clone = Arc::clone(&peer_done);
        let peer = thread::spawn(move || {
            peer_at_swap_clone.wait();
            std::env::set_var("HOME", "/tmp/aoe-peer-swap-sentinel");
            peer_done_clone.wait();
        });

        {
            let guard = isolate_app_dir();
            let guard_path = guard.path().to_path_buf();

            peer_at_swap.wait();
            peer_done.wait();

            assert_eq!(
                std::env::var_os("HOME"),
                Some(OsString::from("/tmp/aoe-peer-swap-sentinel")),
                "peer thread must have swapped HOME by now (Barrier rendezvous)"
            );

            assert_eq!(
                guard.path(),
                guard_path,
                "guard.path() must remain the snapshotted path even after a peer env swap"
            );
        }

        peer.join().expect("peer thread must not panic");

        assert_eq!(
            std::env::var_os("HOME"),
            before_home,
            "guard Drop must restore the pre-construction HOME even when a peer thread swapped it mid-scope"
        );
    }

    /// `isolate_app_dir_at` reads a caller-owned path and MUST NOT own
    /// or delete it: after the guard drops, the caller's directory is
    /// still on disk (only env vars are restored). A refactor that
    /// silently starts moving the caller's `TempDir` into the guard
    /// would delete the dir on Drop and pass every other test in this
    /// module.
    #[test]
    #[serial]
    fn app_dir_guard_at_preserves_caller_tempdir() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.exists(), "precondition: caller tempdir exists");

        {
            let guard = isolate_app_dir_at(&path);
            assert_eq!(
                guard.path(),
                path.as_path(),
                "guard.path() must reflect the caller-provided path, not a fresh tempdir"
            );
        }

        assert!(
            path.exists(),
            "isolate_app_dir_at must not own or delete the caller-provided tempdir on Drop"
        );
    }
}

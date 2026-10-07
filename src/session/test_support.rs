use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// True while *this* thread already owns [`ENV_LOCK`] through an outer
    /// guard. A nested guard on the same thread must not try to re-lock the
    /// non-reentrant `Mutex` (that would deadlock the thread against
    /// itself); it inherits the outer guard's exclusion instead and
    /// acquires nothing. Same-thread nesting is race-free by construction,
    /// so skipping the re-lock loses no safety.
    static ENV_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}

// Acquire [`ENV_LOCK`] unless this thread already holds it.
fn acquire_env_lock() -> Option<MutexGuard<'static, ()>> {
    if ENV_LOCK_HELD.with(Cell::get) {
        None
    } else {
        let guard = lock_reporting_contention(&ENV_LOCK, tests::report_env_lock_contention)
            .unwrap_or_else(PoisonError::into_inner);
        ENV_LOCK_HELD.with(|held| held.set(true));
        Some(guard)
    }
}

pub(crate) fn lock_reporting_contention<'a, T>(
    lock: &'a std::sync::Mutex<T>,
    contended: impl FnOnce(),
) -> std::sync::LockResult<MutexGuard<'a, T>> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Err(error),
        Err(std::sync::TryLockError::WouldBlock) => {
            contended();
            lock.lock()
        }
    }
}

pub(crate) fn write_reporting_contention<'a, T>(
    lock: &'a std::sync::RwLock<T>,
    contended: impl FnOnce(),
) -> std::sync::LockResult<std::sync::RwLockWriteGuard<'a, T>> {
    match lock.try_write() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Err(error),
        Err(std::sync::TryLockError::WouldBlock) => {
            contended();
            lock.write()
        }
    }
}

// RAII guard: snapshots an arbitrary set of env vars, applies the requested mutation, and restores
// the prior state on `Drop` (`set_var` when the var was previously set, `remove_var` when it was
// previously unset).
#[must_use = "EnvGuard restores env vars on Drop; bind it to `_guard`, not `_`, or the override ends on the same line and the test body runs against the caller's real env"]
pub(crate) struct EnvGuard {
    prev: Vec<(&'static str, Option<OsString>)>,
    _lock: Option<MutexGuard<'static, ()>>,
}

impl EnvGuard {
    pub(crate) fn set<V: AsRef<OsStr>>(pairs: &[(&'static str, V)]) -> Self {
        let mut guard = Self {
            prev: Vec::with_capacity(pairs.len()),
            _lock: acquire_env_lock(),
        };
        for (key, value) in pairs {
            guard.snapshot(key);
            // SAFETY (staged for Rust 2024 edition migration): same
            // invariant as [`restore_or_remove`] below.
            std::env::set_var(key, value.as_ref());
        }
        guard
    }

    pub(crate) fn unset(keys: &[&'static str]) -> Self {
        let mut guard = Self {
            prev: Vec::with_capacity(keys.len()),
            _lock: acquire_env_lock(),
        };
        for key in keys {
            guard.snapshot(key);
            // SAFETY (staged for Rust 2024 edition migration): same
            // invariant as [`restore_or_remove`] below.
            std::env::remove_var(key);
        }
        guard
    }

    pub(crate) fn and_set<V: AsRef<OsStr>>(mut self, key: &'static str, value: V) -> Self {
        self.snapshot(key);
        // SAFETY (staged for Rust 2024 edition migration): same
        // invariant as [`restore_or_remove`] below.
        std::env::set_var(key, value.as_ref());
        self
    }

    pub(crate) fn read_lock() -> Self {
        Self {
            prev: Vec::new(),
            _lock: acquire_env_lock(),
        }
    }

    fn snapshot(&mut self, key: &'static str) {
        self.prev.push((key, std::env::var_os(key)));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.prev.drain(..).rev() {
            restore_or_remove(key, prev);
        }
        if self._lock.is_some() {
            ENV_LOCK_HELD.with(|held| held.set(false));
        }
    }
}

pub(crate) struct FavoritesFirstGuard {
    previous: bool,
    _env: EnvGuard,
}

impl FavoritesFirstGuard {
    pub(crate) fn new() -> Self {
        let env = EnvGuard::read_lock();
        Self {
            previous: crate::session::favorites_first(),
            _env: env,
        }
    }
}

impl Drop for FavoritesFirstGuard {
    fn drop(&mut self) {
        crate::session::set_favorites_first(self.previous);
    }
}

pub(crate) fn path_prepended(dir: &Path) -> EnvGuard {
    let guard = EnvGuard::read_lock();
    let inherited = std::env::var_os("PATH").filter(|value| !value.is_empty());
    let path = std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(inherited.iter().flat_map(std::env::split_paths)),
    )
    .expect("join test PATH");
    guard.and_set("PATH", path)
}

pub(crate) fn install_login_shell_path_command(root: &Path, name: &str, script: &str) -> EnvGuard {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create test bin directory");
    let executable = bin.join(name);
    std::fs::write(&executable, script).expect("write test command");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("make test command executable");
    }

    let bin_text = bin.to_str().expect("temporary test path must be UTF-8");
    std::fs::write(
        root.join(".profile"),
        format!(
            "export PATH={}:$PATH\n",
            crate::session::environment::shell_escape(bin_text)
        ),
    )
    .expect("write test login profile");
    path_prepended(&bin).and_set("SHELL", OsString::from("/bin/sh"))
}

// Restores the process-global tied-worktree setting even when a test panics.
#[must_use = "TieWorkdirToNameGuard restores config on Drop"]
pub(crate) struct TieWorkdirToNameGuard {
    previous: bool,
    _lock: Option<MutexGuard<'static, ()>>,
}

impl TieWorkdirToNameGuard {
    pub(crate) fn set(enabled: bool) -> Self {
        let lock = acquire_env_lock();
        let previous = super::config::load_config()
            .ok()
            .flatten()
            .unwrap_or_default()
            .session
            .tie_workdir_to_name;
        let guard = Self {
            previous,
            _lock: lock,
        };
        super::config::update_config(|config| {
            config.session.tie_workdir_to_name = enabled;
        })
        .unwrap();
        guard
    }
}

impl Drop for TieWorkdirToNameGuard {
    fn drop(&mut self) {
        if let Err(error) = super::config::update_config(|config| {
            config.session.tie_workdir_to_name = self.previous;
        }) {
            tracing::warn!(
                target: "session.test",
                "failed to restore tie_workdir_to_name after test: {error}"
            );
        }
        if self._lock.is_some() {
            ENV_LOCK_HELD.with(|held| held.set(false));
        }
    }
}

// RAII guard: isolates `HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` for one test; restores them
// on `Drop`.
#[must_use = "AppDirGuard restores env vars on Drop; bind it to `_tmp` or `_guard`, not `_`, or the isolation ends on the same line and the test body runs against the caller's real env"]
pub(crate) struct AppDirGuard {
    _env: EnvGuard,
    path: PathBuf,
    _temp: Option<TempDir>,
}

impl AppDirGuard {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for AppDirGuard {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

fn restore_or_remove(key: &str, prev: Option<OsString>) {
    // SAFETY (staged for Rust 2024 edition migration, at which point
    // `std::env::set_var` and `std::env::remove_var` become `unsafe fn`):
    // this function mutates a process-global env slot. It is sound to
    // call as long as no other thread is concurrently reading or writing
    // the same env key. The invariant is enforced by:
    //   1. Every `EnvGuard` (and `AppDirGuard`, which delegates to it)
    //      holds [`ENV_LOCK`] for its whole lifetime, so the whole call
    //      sequence (snapshot -> set_var -> test body -> Drop ->
    //      restore_or_remove) is linearized against every other guard in
    //      the process. This is structural, not annotation-dependent: it
    //      no longer matters whether a call site carries `#[serial]` or a
    //      matching `serial_test::serial(...)` group, because the mutex
    //      excludes guards across every group (and across un-annotated
    //      tests). `EnvGuard` shims a caller-chosen key (today also
    //      `CODEX_HOME`, `GEMINI_CLI_HOME`, `CLAUDE_CONFIG_DIR`, the sibling
    //      `*_CONFIG_DIR` overrides, and `AOE_MOUSE_CAPTURE`), so no fixed grep
    //      list can bound which readers might race. Routing every env mutation
    //      through the guard is what keeps that open set safe.
    //   2. The `#[tokio::test]` sites that use this helper all run on
    //      the default single-threaded runtime; no worker task reads env
    //      concurrently with the mutation.
    //   3. Raw `std::env::set_var` calls that bypass the guard entirely
    //      (a peer thread, a not-yet-migrated test) are outside this
    //      lock and can still race; the guard only protects code that
    //      goes through it. Prefer `EnvGuard` / `isolate_home` for any
    //      new env mutation in tests.
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

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
    // Keep the watched native-state ancestor stable when XDG data is first used.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::fs::create_dir_all(path.join(".local/share"))
        .expect("create isolated XDG data directory before exposing HOME");
    // Only the vars this target actually mutates are handed to the guard; a var that is never
    // written needs no restore.
    #[allow(unused_mut)]
    let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", path.clone())];
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        pairs.push(("XDG_CONFIG_HOME", path.join(".config")));
        pairs.push(("XDG_DATA_HOME", path.join(".local/share")));
    }
    AppDirGuard {
        _env: EnvGuard::set(&pairs),
        path,
        _temp: temp,
    }
}

pub(crate) type HomeGuard = EnvGuard;

pub(crate) fn isolate_home(temp: &Path) -> HomeGuard {
    EnvGuard::set(&[
        ("HOME", temp.to_path_buf()),
        ("XDG_CONFIG_HOME", temp.join(".config")),
    ])
}

/// Captures every event emitted on the current thread until dropped.
///
/// Scoped on purpose: a process-global capture subscriber serializes every
/// thread's logging behind one lock, stalling unrelated tests.
pub(crate) struct LogCapture {
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

#[derive(Clone)]
struct LogCaptureWriter(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    pub(crate) fn start() -> Self {
        use tracing_subscriber::layer::SubscriberExt;
        let buf = std::sync::Arc::default();
        let writer = LogCaptureWriter(std::sync::Arc::clone(&buf));
        let subscriber = tracing_subscriber::Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false),
        );
        let guard = tracing::subscriber::set_default(subscriber);
        // Callsites cached as disabled by another subscriber must be re-evaluated.
        tracing::callsite::rebuild_interest_cache();
        Self { buf, _guard: guard }
    }

    pub(crate) fn contents(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap_or_else(PoisonError::into_inner))
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::panic::AssertUnwindSafe;

    struct AmbientEnvRestore {
        key: &'static str,
        previous: Option<OsString>,
        _guard: EnvGuard,
    }

    impl Drop for AmbientEnvRestore {
        fn drop(&mut self) {
            restore_or_remove(self.key, self.previous.take());
        }
    }

    impl AmbientEnvRestore {
        fn capture(key: &'static str) -> Self {
            let guard = EnvGuard::read_lock();
            Self {
                key,
                previous: std::env::var_os(key),
                _guard: guard,
            }
        }
    }

    thread_local! {
        static LOCK_WAITING: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn report_env_lock_contention() {
        LOCK_WAITING.with_borrow_mut(|waiting| {
            if let Some(waiting) = waiting.take() {
                let _ = waiting.send(());
            }
        });
    }

    #[test]
    fn env_lock_orders_readers_and_path_derivation() {
        use std::sync::mpsc;
        use std::time::Duration;

        for derive_path in [false, true] {
            let writer = EnvGuard::read_lock();
            let ambient = std::env::var_os("PATH");
            let inherited: Vec<_> = ambient
                .iter()
                .filter(|v| !v.is_empty())
                .flat_map(std::env::split_paths)
                .collect();
            let peer = TempDir::new().unwrap();
            let shim = TempDir::new().unwrap();
            let scrubbed = std::env::join_paths(
                std::iter::once(peer.path().to_path_buf()).chain(inherited.iter().cloned()),
            )
            .unwrap();
            let writer = writer.and_set("PATH", scrubbed);
            let expected = if derive_path {
                Some(
                    std::env::join_paths(
                        std::iter::once(shim.path().to_path_buf()).chain(inherited),
                    )
                    .unwrap(),
                )
            } else {
                ambient
            };
            let (waiting_tx, waiting_rx) = mpsc::channel();
            let (observed_tx, observed_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let (contended, observed, exclusive) = std::thread::scope(|scope| {
                let shim = shim.path();
                let reader = scope.spawn(move || {
                    LOCK_WAITING.with_borrow_mut(|waiting| *waiting = Some(waiting_tx));
                    let _guard = if derive_path {
                        path_prepended(shim)
                    } else {
                        EnvGuard::read_lock()
                    };
                    observed_tx.send(std::env::var_os("PATH")).unwrap();
                    let _ = release_rx.recv();
                });
                let contended = waiting_rx.recv_timeout(Duration::from_secs(30));
                drop(writer);
                let observed = observed_rx.recv_timeout(Duration::from_secs(30));
                let exclusive = matches!(
                    ENV_LOCK.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                );
                drop(release_tx);
                reader.join().unwrap();
                (contended, observed, exclusive)
            });
            contended.expect("reader must encounter the held writer lock");
            assert_eq!(observed.unwrap(), expected, "derive_path={derive_path}");
            assert!(
                exclusive,
                "the reader must retain exclusion until its guard drops"
            );
        }
    }

    // Drop restores each key's exact prior state: a non-UTF-8 value byte-for-byte (#2751), empty
    // versus unset, a value `unset` removed, and a key set twice to its pre-guard value.
    #[test]
    #[serial]
    fn env_guard_drop_restores_the_exact_prior_state() {
        // (key, prior, values set in one call or None for `unset`, value while live)
        let mut cases: Vec<(
            &'static str,
            Option<OsString>,
            Option<&[&str]>,
            Option<&str>,
        )> = vec![
            (
                "AOE_ENVGUARD_EMPTY",
                Some(OsString::new()),
                Some(&["populated"]),
                Some("populated"),
            ),
            (
                "AOE_ENVGUARD_UNSET",
                None,
                Some(&["populated"]),
                Some("populated"),
            ),
            ("AOE_ENVGUARD_UNSET_ME", Some("original".into()), None, None),
            (
                "AOE_ENVGUARD_DUP",
                Some("original".into()),
                Some(&["first", "second"]),
                Some("second"),
            ),
        ];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let non_utf8 = OsString::from(OsStr::from_bytes(b"/tmp/aoe-\xFF\xFE-home"));
            cases.push((
                "HOME",
                Some(non_utf8),
                Some(&["/tmp/aoe-envguard-scoped"]),
                Some("/tmp/aoe-envguard-scoped"),
            ));
        }
        for (key, prior, values, live) in cases {
            let _restore = AmbientEnvRestore::capture(key);
            match &prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            {
                let _guard = match values {
                    Some(values) => {
                        EnvGuard::set(&values.iter().map(|value| (key, *value)).collect::<Vec<_>>())
                    }
                    None => EnvGuard::unset(&[key]),
                };
                assert_eq!(
                    std::env::var_os(key),
                    live.map(OsString::from),
                    "{key} while live"
                );
            }
            assert_eq!(std::env::var_os(key), prior, "{key} after drop");
        }
    }

    // Drop restores HOME and the XDG roots to their pre-construction snapshot (#2717), after a
    // plain scope, a mid-scope write, and a panicking test body.
    #[test]
    #[serial]
    fn app_dir_guard_drop_restores_env_vars() {
        const KEYS: [&str; 3] = ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"];
        type Body = fn(&AppDirGuard);
        let cases: [(&str, Body, bool); 3] = [
            ("plain", |_| {}, false),
            (
                "mid-scope write",
                |_| std::env::set_var("HOME", "/tmp/aoe-mid-scope-sentinel"),
                false,
            ),
            ("panic", |_| panic!("simulate a test-body panic"), true),
        ];
        for (case, body, panics) in cases {
            let _restore = KEYS.map(AmbientEnvRestore::capture);
            let before = KEYS.map(std::env::var_os);
            let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let guard = isolate_app_dir();
                assert_eq!(
                    std::env::var_os("HOME"),
                    Some(guard.path().into()),
                    "{case}"
                );
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                for (key, sub) in [
                    ("XDG_CONFIG_HOME", ".config"),
                    ("XDG_DATA_HOME", ".local/share"),
                ] {
                    assert_eq!(
                        std::env::var_os(key),
                        Some(guard.path().join(sub).into()),
                        "{case}"
                    );
                }
                body(&guard);
            }));
            assert_eq!(unwound.is_err(), panics, "{case}");
            assert_eq!(KEYS.map(std::env::var_os), before, "{case}");
        }
    }

    // `isolate_app_dir_at` reads a caller-owned path and MUST NOT own or delete it: after the guard
    // drops, the caller's directory is still on disk (only env vars are restored).
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

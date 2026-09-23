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

#[must_use = "AutoResumeGuard restores config on Drop"]
pub(crate) struct AutoResumeGuard {
    previous: bool,
    _lock: Option<MutexGuard<'static, ()>>,
}

impl AutoResumeGuard {
    pub(crate) fn set(enabled: bool) -> Self {
        let lock = acquire_env_lock();
        let previous = super::config::load_config()
            .ok()
            .flatten()
            .unwrap_or_default()
            .session
            .auto_resume_on_restart;
        let guard = Self {
            previous,
            _lock: lock,
        };
        super::config::update_config(|config| {
            config.session.auto_resume_on_restart = enabled;
        })
        .unwrap();
        guard
    }
}

impl Drop for AutoResumeGuard {
    fn drop(&mut self) {
        if let Err(error) = super::config::update_config(|config| {
            config.session.auto_resume_on_restart = self.previous;
        }) {
            tracing::warn!(target: "session.test", "failed to restore auto_resume_on_restart: {error}");
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

    // Locks: a non-UTF-8 prior value MUST round-trip through the guard byte-for-byte.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn env_guard_drop_restores_non_utf8_prior_value() {
        use std::os::unix::ffi::OsStrExt;

        let _restore = AmbientEnvRestore::capture("HOME");

        let non_utf8 = OsString::from(OsStr::from_bytes(b"/tmp/aoe-\xFF\xFE-home"));
        assert!(
            non_utf8.to_str().is_none(),
            "precondition: the seeded value must not be valid UTF-8"
        );
        std::env::set_var("HOME", &non_utf8);
        assert!(
            std::env::var("HOME").is_err(),
            "precondition: env::var must surface NotUnicode, the error the old \
             Option<String> snapshot swallowed via .ok()"
        );

        {
            let _guard = EnvGuard::set(&[("HOME", "/tmp/aoe-envguard-scoped")]);
            assert_eq!(
                std::env::var_os("HOME"),
                Some(OsString::from("/tmp/aoe-envguard-scoped")),
                "guard must apply its override while live"
            );
        }

        assert_eq!(
            std::env::var_os("HOME"),
            Some(non_utf8),
            "Drop must restore the non-UTF-8 prior value byte-for-byte rather than remove it (#2751)"
        );
    }

    #[test]
    #[serial]
    fn env_guard_drop_preserves_empty_versus_unset() {
        let _restore_set = AmbientEnvRestore::capture("AOE_ENVGUARD_EMPTY");
        let _restore_unset = AmbientEnvRestore::capture("AOE_ENVGUARD_UNSET");

        std::env::set_var("AOE_ENVGUARD_EMPTY", "");
        std::env::remove_var("AOE_ENVGUARD_UNSET");

        {
            let _guard = EnvGuard::set(&[
                ("AOE_ENVGUARD_EMPTY", "populated"),
                ("AOE_ENVGUARD_UNSET", "populated"),
            ]);
        }

        assert_eq!(
            std::env::var_os("AOE_ENVGUARD_EMPTY"),
            Some(OsString::new()),
            "an empty prior value must be restored as empty, not removed"
        );
        assert_eq!(
            std::env::var_os("AOE_ENVGUARD_UNSET"),
            None,
            "a previously-unset var must be removed on Drop, not set to empty"
        );
    }

    // `EnvGuard::unset` removes the key for the scope and restores the prior value on `Drop`.
    #[test]
    #[serial]
    fn env_guard_unset_removes_then_restores() {
        let _restore = AmbientEnvRestore::capture("AOE_ENVGUARD_UNSET_ME");

        std::env::set_var("AOE_ENVGUARD_UNSET_ME", "original");

        {
            let _guard = EnvGuard::unset(&["AOE_ENVGUARD_UNSET_ME"]);
            assert_eq!(
                std::env::var_os("AOE_ENVGUARD_UNSET_ME"),
                None,
                "unset must remove the var while the guard is live"
            );
        }

        assert_eq!(
            std::env::var_os("AOE_ENVGUARD_UNSET_ME"),
            Some(OsString::from("original")),
            "Drop must restore the value unset removed"
        );
    }

    // A key listed twice in one `set` call must round-trip to its pre-guard value, not to the
    // intermediate write.
    #[test]
    #[serial]
    fn env_guard_drop_restores_duplicate_key_to_pre_guard_value() {
        let _restore = AmbientEnvRestore::capture("AOE_ENVGUARD_DUP");

        std::env::set_var("AOE_ENVGUARD_DUP", "original");

        {
            let _guard = EnvGuard::set(&[
                ("AOE_ENVGUARD_DUP", "first"),
                ("AOE_ENVGUARD_DUP", "second"),
            ]);
            assert_eq!(
                std::env::var_os("AOE_ENVGUARD_DUP"),
                Some(OsString::from("second")),
                "the last write in the pair list wins while the guard is live"
            );
        }

        assert_eq!(
            std::env::var_os("AOE_ENVGUARD_DUP"),
            Some(OsString::from("original")),
            "Drop must restore the pre-guard value, not the intermediate 'first'"
        );
    }

    // Locks the fix for: `Drop` MUST restore `HOME` and (on Linux/macOS) `XDG_CONFIG_HOME` plus
    // `XDG_DATA_HOME` to their pre-guard values.
    #[test]
    #[serial]
    fn app_dir_guard_drop_restores_env_vars() {
        let _home = AmbientEnvRestore::capture("HOME");
        let _xdg = AmbientEnvRestore::capture("XDG_CONFIG_HOME");
        let _data = AmbientEnvRestore::capture("XDG_DATA_HOME");
        let before_home = std::env::var_os("HOME");
        let before_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let before_xdg_data = std::env::var_os("XDG_DATA_HOME");

        {
            let guard = isolate_app_dir();
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

    // Locks the `remove_var` branch of `restore_or_remove`: when the pre-guard env var was unset,
    // `Drop` MUST leave it unset.
    #[test]
    #[serial]
    fn app_dir_guard_drop_removes_env_vars_when_unset() {
        let _restore_home = AmbientEnvRestore::capture("HOME");
        let _restore_xdg = AmbientEnvRestore::capture("XDG_CONFIG_HOME");
        let _restore_xdg_data = AmbientEnvRestore::capture("XDG_DATA_HOME");

        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_DATA_HOME");

        {
            let guard = isolate_app_dir();
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
    }

    // `AsRef<Path>` lets call sites pass `&guard` wherever a `Path`-like is expected, matching
    // `Path::join`-style ergonomics.
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

    // Locks the "Drop-runs-on-unwind" contract that motivates the entire RAII conversion.
    #[test]
    #[serial]
    fn app_dir_guard_drop_restores_env_vars_on_panic() {
        let _home = AmbientEnvRestore::capture("HOME");
        let _xdg = AmbientEnvRestore::capture("XDG_CONFIG_HOME");
        let _data = AmbientEnvRestore::capture("XDG_DATA_HOME");
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

    // Locks the "snapshot at construction" semantic: `Drop` restores to the pre-construction env
    // values, not to whatever the test last wrote inside the guard's scope.
    #[test]
    #[serial]
    fn app_dir_guard_drop_ignores_mid_scope_env_writes() {
        let _home = AmbientEnvRestore::capture("HOME");
        let _xdg = AmbientEnvRestore::capture("XDG_CONFIG_HOME");
        let _data = AmbientEnvRestore::capture("XDG_DATA_HOME");
        let before_home = std::env::var_os("HOME");

        {
            let _guard = isolate_app_dir();
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

    // A peer thread writing `HOME` mid-scope must not survive the guard's `Drop`: `Drop`
    // unconditionally restores the pre-construction snapshot regardless of intervening writes from
    // any thread.
    #[test]
    #[serial]
    fn app_dir_guard_survives_concurrent_peer_env_swap() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let _home = AmbientEnvRestore::capture("HOME");
        let _xdg = AmbientEnvRestore::capture("XDG_CONFIG_HOME");
        let _data = AmbientEnvRestore::capture("XDG_DATA_HOME");
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

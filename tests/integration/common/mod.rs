//! Shared helpers for integration tests.
//!
//! Declared once from `tests/integration/main.rs`; consumers import via
//! `use crate::common::...`.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::Command;

/// Stable per-process tmux socket for integration tests. aoe now resolves an
/// explicit `-S <socket>` (#2608) and caches it once per process, so tests
/// must (a) point aoe at a hermetic socket via `AOE_TMUX_SOCKET` and (b) make
/// their own raw `tmux` calls target the same socket. The path is stable (not
/// per-test-home) precisely because the lib caches it once; a per-home path
/// would be dropped out from under a later test. Referencing it also sets
/// `AOE_TMUX_SOCKET`, so any raw-tmux call site locks the lib onto the same
/// socket before its first lib tmux call. `#[serial]` tests keep the env write
/// single-threaded.
///
/// The name carries this process's pid so it is stable within one integration
/// binary yet never collides with a concurrent integration process (a second
/// `cargo test` run or a leftover server from a prior run). Without the pid,
/// two processes would share one tmux server and interfere, most visibly as
/// root where `/tmp` is shared across every same-uid run.
pub fn tmux_socket() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("aoe-integration-tmux-{}.sock", std::process::id()));
    std::env::set_var("AOE_TMUX_SOCKET", &path);
    path
}

/// Path to the Node ACP test shim used by acp_* integration tests.
pub fn shim_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("acp-worker")
        .join("test-shim")
        .join("shim.mjs")
}

/// Returns `Ok(())` if the structured view shim can be spawned (Node available, shim
/// file present, shim deps installed). Otherwise returns a short reason
/// that callers print before skipping. CI installs deps via `npm ci` in
/// `acp-worker/test-shim/` before running the integration leg; local
/// runs need the same one-shot setup, which the message points at.
pub fn shim_ready() -> Result<(), String> {
    shim_node()?;
    let shim = shim_path();
    if !shim.exists() {
        return Err(format!("shim missing at {}", shim.display()));
    }
    let node_modules = shim.parent().unwrap().join("node_modules");
    if !node_modules.exists() {
        return Err(
            "shim deps not installed; run `cd acp-worker/test-shim && npm ci` first".into(),
        );
    }
    Ok(())
}

/// Resolve the runtime behind version-manager launchers before tests isolate
/// HOME or the product filters the child environment. Probe and spawn must use
/// the same executable, not re-enter a launcher without its configuration.
pub fn shim_node() -> Result<&'static Path, String> {
    static NODE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    NODE.get_or_init(|| {
        let output = std::process::Command::new("node")
            .args(["--print", "process.execPath"])
            .output()
            .map_err(|error| format!("cannot resolve Node runtime: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cannot resolve Node runtime: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|error| format!("invalid Node runtime path: {error}"))?;
        std::fs::canonicalize(path.trim())
            .map_err(|error| format!("cannot resolve Node executable: {error}"))
    })
    .as_ref()
    .map(PathBuf::as_path)
    .map_err(Clone::clone)
}

/// True when the effective uid is 0. Root bypasses the Unix DAC permission
/// bits, so a test that injects a write failure by making a dir read-only
/// cannot make the write fail and must skip rather than assert `is_err()`.
#[cfg(unix)]
pub fn running_as_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Set `HOME` (and `XDG_CONFIG_HOME` on Linux/macOS) to a fresh temp dir so
/// tests read and write to isolated state. Returns the guard; drop it to clean
/// up.
///
/// # Safety caveat
/// `set_var` is not thread-safe. Callers must be `#[serial]`.
pub fn setup_temp_home() -> TestHome {
    let temp = TempDir::new().unwrap();
    let env = set_temp_home(temp.path());
    TestHome { env, temp }
}

/// Restore environment before the caller drops its temporary directory.
pub fn set_temp_home(path: &Path) -> EnvGuard {
    let mut env = EnvGuard::new(&["HOME", "XDG_CONFIG_HOME", "AOE_TMUX_SOCKET"]);
    let _ = tmux_socket();
    env.set("HOME", path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    env.set("XDG_CONFIG_HOME", path.join(".config"));
    env
}

pub struct CwdGuard(PathBuf);

impl CwdGuard {
    pub fn set(path: &Path) -> Self {
        let guard = Self(std::env::current_dir().expect("original cwd"));
        std::env::set_current_dir(path).expect("test cwd");
        guard
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore cwd");
    }
}

#[must_use]
pub struct EnvGuard {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub fn from_pairs(pairs: &[(&'static str, &'static str)]) -> Self {
        let mut guard = Self::new(&[]);
        for (key, value) in pairs {
            guard.set(key, value);
        }
        guard
    }
    pub fn new(keys: &[&'static str]) -> Self {
        Self {
            vars: keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        }
    }

    pub fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        if !self.vars.iter().any(|(saved, _)| *saved == key) {
            self.vars.push((key, std::env::var_os(key)));
        }
        std::env::set_var(key, value);
    }

    pub fn and_set(mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.set(key, value);
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, old) in self.vars.drain(..).rev() {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[must_use]
pub struct TestHome {
    pub env: EnvGuard,
    temp: TempDir,
}

impl TestHome {
    pub fn path(&self) -> &Path {
        self.temp.path()
    }
}

/// A live `aoe __acp-runner` whose agent is the Node ACP shim.
///
/// Before #2977 these tests fronted the shim with a hand-rolled byte proxy on
/// a unix socket, which was a fair stand-in while the daemon spoke raw ACP
/// over `<id>.sock`. That socket is gone: the daemon now speaks the typed
/// control protocol, and a byte proxy cannot answer it. Rather than
/// reimplement the runner side in the fixture, spawn the real runner. It
/// costs a process and gives the attach path genuine end-to-end coverage
/// instead of a mock of the peer it is being tested against.
///
/// Returns the `--socket` path (still the derivation base for the control
/// sibling, which is what `AcpClient::attach` dials) and guards that keep the
/// temp dir and the runner process alive for the test's duration.
pub async fn spawn_runner_with_shim(
    session_id: &str,
    env: &[(&str, String)],
) -> (PathBuf, RunnerGuard) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let xdg = temp.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    // `--socket` is an explicit path, so point it straight at the temp dir
    // rather than deriving the app-dir layout (which varies by platform and
    // by whether XDG_CONFIG_HOME is set). The runner still writes its
    // registry record under the temp HOME; nothing here reads it.
    //
    // `session_id` must match what the caller passes to `AcpClient::attach`:
    // the daemon verifies the id the runner announces in its `Hello`, so a
    // fixture that spawned under a fixed id would be rejected.
    let socket_path = temp.path().join(format!("{session_id}.sock"));
    let control = temp.path().join(format!("{session_id}.control.sock"));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aoe"));
    cmd.args([
        "__acp-runner",
        "--socket",
        socket_path.to_str().unwrap(),
        "--session-id",
        session_id,
        "--agent-name",
        "shim",
        "--cwd",
        home.to_str().unwrap(),
        "--",
        shim_node().expect("shim prerequisite").to_str().unwrap(),
        shim_path().to_str().unwrap(),
    ])
    .env("HOME", &home)
    .env("XDG_CONFIG_HOME", &xdg)
    .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Preseeded sessions need one initial load before testing a later attach.
    if env.iter().any(|(key, _)| *key == "SHIM_PRESEED_SESSION_ID") {
        cmd.env("SHIM_LOAD_SESSION", "1");
    }
    let child = cmd.spawn().expect("spawn acp runner");

    // The runner binds the control socket before spawning the agent, so its
    // appearance is the readiness signal the daemon's own probe uses.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control.exists() {
        assert!(
            Instant::now() < deadline,
            "runner never bound {}",
            control.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Resume attaches to an established runner, not merely a preseeded agent.
    // Prime the runner cache exactly as the original daemon would have done.
    {
        use agent_of_empires::acp::control_protocol::{self, ControlBody};
        let mut initial = tokio::net::UnixStream::connect(&control).await.unwrap();
        assert!(matches!(
            control_protocol::read_frame(&mut initial).await.unwrap(),
            Some(ControlBody::Hello { .. })
        ));
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::Attach {
                control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::Initialize {
                request: serde_json::json!({"protocolVersion": 1}),
            },
        )
        .await
        .unwrap();
        loop {
            match control_protocol::read_frame(&mut initial).await.unwrap() {
                Some(ControlBody::Initialized { .. }) => break,
                Some(ControlBody::Notify { .. }) => {}
                frame => panic!("initial initialize failed: {frame:?}"),
            }
        }
        let preseed = env
            .iter()
            .find(|(key, _)| *key == "SHIM_PRESEED_SESSION_ID");
        let (method, request) = match preseed {
            Some((_, id)) => (
                "session/load",
                serde_json::json!({"sessionId": id, "cwd": home, "mcpServers": []}),
            ),
            None => (
                "session/new",
                serde_json::json!({"cwd": home, "mcpServers": []}),
            ),
        };
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::EstablishSession {
                method: method.into(),
                request,
            },
        )
        .await
        .unwrap();
        loop {
            match control_protocol::read_frame(&mut initial).await.unwrap() {
                Some(ControlBody::SessionReady { .. }) => break,
                Some(ControlBody::Notify { .. }) => {}
                frame => panic!("initial session establishment failed: {frame:?}"),
            }
        }
    }

    (
        socket_path,
        RunnerGuard {
            _child: child,
            _temp: temp,
        },
    )
}

/// Keeps the runner process and its temp HOME alive for the test. Dropping
/// it kills the runner (`kill_on_drop`), which takes the shim with it.
pub struct RunnerGuard {
    _child: tokio::process::Child,
    _temp: tempfile::TempDir,
}

/// Bind ephemeral, drop, return the port. Tiny TOCTOU window before the
/// caller binds; acceptable under `#[serial]`. Used by every integration
/// test that spawns an `aoe serve` subprocess.
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// Poll-connect against `127.0.0.1:port` until success or `deadline`
/// elapses. Returns `true` on success, `false` on timeout. The 100ms
/// inner sleep matches the rest of the test harness; the connect timeout
/// is shorter so the deadline budget is mostly spent retrying rather
/// than blocked on a single slow connect.
pub fn wait_for_port(port: u16, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

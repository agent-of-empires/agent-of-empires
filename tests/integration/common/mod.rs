//! Shared helpers for integration tests, declared from `main.rs`.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::Command;

/// Hermetic tmux socket shared by the lib and by raw `tmux` calls, and set on
/// `AOE_TMUX_SOCKET` as a side effect. aoe caches the socket once per process,
/// so the path is per-process (pid-named, never per test home) and `#[serial]`
/// callers keep the env write single-threaded.
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

/// `Ok(())` when the structured view shim can be spawned; otherwise a reason
/// callers print before skipping.
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
/// HOME: probe and spawn must use the same executable.
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

/// Point `HOME` (and `XDG_CONFIG_HOME`) at a fresh temp dir; drop the guard to
/// restore. `set_var` is not thread-safe, so callers must be `#[serial]`.
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

/// A live `aoe __acp-runner` whose agent is the Node ACP shim: the real runner
/// rather than a mock, since the daemon speaks the typed control protocol.
///
/// Returns the `--socket` path, from which `AcpClient::attach` derives the
/// control sibling, and a guard holding the runner and its temp dir open.
pub async fn spawn_runner_with_shim(
    session_id: &str,
    env: &[(&str, String)],
) -> (PathBuf, RunnerGuard) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let xdg = temp.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    // The daemon verifies the id the runner announces, so `session_id` must
    // match what the caller later attaches with.
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

/// Dropping this kills the runner, which takes the shim with it.
pub struct RunnerGuard {
    _child: tokio::process::Child,
    _temp: tempfile::TempDir,
}

/// Bind ephemeral, drop, return the port. The TOCTOU window before the caller
/// binds is acceptable under `#[serial]`.
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// Poll-connect `127.0.0.1:port` until it succeeds or `deadline` elapses.
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

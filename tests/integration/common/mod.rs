//! Shared helpers for integration tests, declared from `main.rs`.

#[cfg(debug_assertions)]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(debug_assertions)]
use std::time::{Duration, Instant};
use tempfile::TempDir;
#[cfg(debug_assertions)]
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

/// Keeps a spawned or in-process aoe off the developer's systemd user manager.
pub const SYSTEMD_OFF: &str = "[systemd]\nenabled = false\n";

/// Appends [`SYSTEMD_OFF`] to the `config.toml` in `app_dir`.
pub fn disable_systemd_scopes(app_dir: &Path) {
    use std::io::Write;
    std::fs::create_dir_all(app_dir).expect("create app dir");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dir.join("config.toml"))
        .expect("open config.toml");
    writeln!(file, "\n{SYSTEMD_OFF}").expect("write config.toml");
}

/// Point `HOME` (and `XDG_CONFIG_HOME`) at a fresh temp dir; drop the guard to
/// restore. `set_var` is not thread-safe, so callers must be `#[serial]`.
///
/// Rooted in `/tmp` on macOS because the Daemon socket lives under the home and
/// `$TMPDIR` overflows the 104-byte `sun_path` limit. Elsewhere the default root
/// is kept, since fixtures execute files under the home and `/tmp` may be noexec.
pub fn setup_temp_home() -> TestHome {
    #[cfg(target_os = "macos")]
    let temp = TempDir::new_in("/tmp").unwrap();
    #[cfg(not(target_os = "macos"))]
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
    #[cfg(debug_assertions)]
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
#[cfg(debug_assertions)]
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
    let mut child = cmd.spawn().expect("spawn acp runner");

    // Resume attaches to an established runner, not merely a preseeded agent.
    // Prime the runner cache exactly as the original daemon would have done.
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let exited = || {
        child
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    };
    if let Err(error) = bootstrap_runner(&control, deadline, exited, method, request).await {
        panic!("runner {session_id} didn't start: {error}");
    }

    (
        socket_path,
        RunnerGuard {
            _child: child,
            _temp: temp,
        },
    )
}

/// Connects to a runner's control socket and takes the connection through
/// SessionReady, all before `deadline`. `exited` returns the runner's exit
/// status once it has exited; it is checked before every connect attempt.
#[cfg(debug_assertions)]
async fn bootstrap_runner(
    control: &Path,
    deadline: tokio::time::Instant,
    mut exited: impl FnMut() -> Option<String>,
    method: &str,
    request: serde_json::Value,
) -> Result<(), String> {
    use agent_of_empires::acp::control_protocol::{self, ControlBody};
    use std::io::ErrorKind;

    let mut last_error = None;
    let mut stream = loop {
        if let Some(status) = exited() {
            return Err(format!(
                "the runner exited ({status}) before accepting a control connection; last connect error: {last_error:?}"
            ));
        }
        match tokio::net::UnixStream::connect(control).await {
            Ok(stream) => break stream,
            // Not bound yet, or bound but not listening yet.
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                last_error = Some(error)
            }
            Err(error) => return Err(format!("connecting to {}: {error}", control.display())),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out connecting to {}; last connect error: {last_error:?}",
                control.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let stream = &mut stream;
    expect_frame(stream, deadline, &mut exited, "Hello", false, |frame| {
        matches!(frame, ControlBody::Hello { .. })
    })
    .await?;
    send_frame(
        stream,
        deadline,
        "Attach",
        ControlBody::Attach {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
        },
    )
    .await?;
    send_frame(
        stream,
        deadline,
        "Initialize",
        ControlBody::Initialize {
            request: serde_json::json!({"protocolVersion": 1}),
        },
    )
    .await?;
    expect_frame(
        stream,
        deadline,
        &mut exited,
        "Initialized",
        true,
        |frame| matches!(frame, ControlBody::Initialized { .. }),
    )
    .await?;
    send_frame(
        stream,
        deadline,
        "EstablishSession",
        ControlBody::EstablishSession {
            method: method.into(),
            request,
        },
    )
    .await?;
    expect_frame(
        stream,
        deadline,
        &mut exited,
        "SessionReady",
        true,
        |frame| matches!(frame, ControlBody::SessionReady { .. }),
    )
    .await
}

#[cfg(debug_assertions)]
async fn send_frame(
    stream: &mut tokio::net::UnixStream,
    deadline: tokio::time::Instant,
    stage: &str,
    body: agent_of_empires::acp::control_protocol::ControlBody,
) -> Result<(), String> {
    let write = agent_of_empires::acp::control_protocol::write_frame(stream, &body);
    match tokio::time::timeout_at(deadline, write).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("sending {stage}: {error:#}")),
        Err(_) => Err(format!("timed out sending {stage}")),
    }
}

/// Reads frames until `wanted` matches, skipping notifications if `skip_notify`.
#[cfg(debug_assertions)]
async fn expect_frame(
    stream: &mut tokio::net::UnixStream,
    deadline: tokio::time::Instant,
    exited: &mut impl FnMut() -> Option<String>,
    stage: &str,
    skip_notify: bool,
    wanted: impl Fn(&agent_of_empires::acp::control_protocol::ControlBody) -> bool,
) -> Result<(), String> {
    use agent_of_empires::acp::control_protocol::{self, ControlBody};
    loop {
        let read = control_protocol::read_frame(stream);
        let frame = match tokio::time::timeout_at(deadline, read).await {
            Err(_) => return Err(format!("timed out waiting for {stage}")),
            Ok(Err(error)) => format!("{error:#}"),
            Ok(Ok(Some(frame))) if wanted(&frame) => return Ok(()),
            Ok(Ok(Some(ControlBody::Notify { .. }))) if skip_notify => continue,
            Ok(Ok(frame)) => format!("{frame:?}"),
        };
        let runner = exited().unwrap_or_else(|| "still running".into());
        return Err(format!("expected {stage}, got {frame} (runner: {runner})"));
    }
}

#[cfg(debug_assertions)]
/// Dropping this kills the runner, which takes the shim with it.
pub struct RunnerGuard {
    _child: tokio::process::Child,
    _temp: tempfile::TempDir,
}

#[cfg(debug_assertions)]
/// Bind ephemeral, drop, return the port. The TOCTOU window before the caller
/// binds is acceptable under `#[serial]`.
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

#[cfg(debug_assertions)]
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

#[cfg(debug_assertions)]
mod tests {
    use super::*;
    use agent_of_empires::acp::control_protocol::{self, ControlBody, CONTROL_PROTOCOL_VERSION};
    use tokio::net::UnixListener;

    enum Peer {
        Completes,
        Stalls,
        OpensWith(ControlBody),
    }

    async fn fake_runner(listener: UnixListener, peer: Peer) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello = ControlBody::Hello {
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            session_id: "s".into(),
        };
        let notify = ControlBody::Notify {
            method: "session/update".into(),
            params: serde_json::json!({}),
        };
        let mut script = match peer {
            Peer::Stalls => return std::future::pending().await,
            Peer::OpensWith(frame) => vec![(0, frame)],
            // (frames to read first, frame to send)
            Peer::Completes => vec![
                (0, hello),
                (2, notify.clone()),
                (
                    0,
                    ControlBody::Initialized {
                        result: serde_json::json!({}),
                    },
                ),
                (1, notify),
                (
                    0,
                    ControlBody::SessionReady {
                        acp_session_id: "acp".into(),
                        result: serde_json::json!({}),
                    },
                ),
            ],
        };
        for (reads, frame) in script.drain(..) {
            for _ in 0..reads {
                control_protocol::read_frame(&mut stream)
                    .await
                    .unwrap()
                    .unwrap();
            }
            control_protocol::write_frame(&mut stream, &frame)
                .await
                .unwrap();
        }
        // Hold the connection open, as a runner does.
        std::future::pending::<()>().await;
    }

    async fn bootstrap(
        control: &Path,
        within: Duration,
        exited: impl FnMut() -> Option<String>,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + within;
        let bootstrap = bootstrap_runner(
            control,
            deadline,
            exited,
            "session/new",
            serde_json::json!({}),
        );
        // A regression that loses the deadline fails here instead of hanging the suite.
        tokio::time::timeout(within + Duration::from_secs(5), bootstrap)
            .await
            .expect("bootstrap overran its own deadline")
    }

    #[tokio::test]
    async fn bootstrap_waits_for_a_runner_that_listens_after_creating_its_socket() {
        let temp = tempfile::tempdir().unwrap();
        let control = temp.path().join("c.sock");
        // A socket file nobody listens on refuses connections, as a runner
        // between bind and listen does.
        drop(std::os::unix::net::UnixListener::bind(&control).unwrap());
        let mut checks = 0;
        let exited = || {
            checks += 1;
            // Listen only after at least one connect attempt was refused.
            if checks == 2 {
                std::fs::remove_file(&control).unwrap();
                let listener = UnixListener::bind(&control).unwrap();
                tokio::spawn(fake_runner(listener, Peer::Completes));
            }
            None
        };
        bootstrap(&control, Duration::from_secs(10), exited)
            .await
            .unwrap();
        assert!(checks >= 2, "{checks}");
    }

    #[tokio::test]
    async fn bootstrap_failures_name_their_stage() {
        // (runner, budget, error starts with)
        let cases = [
            // Exits before it ever binds.
            (
                None,
                Duration::from_secs(10),
                "the runner exited (exit status: 3)",
            ),
            // Connected, then never says Hello: the budget covers the handshake too.
            (
                Some(Peer::Stalls),
                Duration::from_millis(300),
                "timed out waiting for Hello",
            ),
            // A protocol error fails at once rather than at the deadline.
            (
                Some(Peer::OpensWith(ControlBody::Initialized {
                    result: serde_json::json!({}),
                })),
                Duration::from_secs(10),
                "expected Hello, got Some(Initialized",
            ),
            // A runner says Hello before anything else, notifications included.
            (
                Some(Peer::OpensWith(ControlBody::Notify {
                    method: "session/update".into(),
                    params: serde_json::json!({}),
                })),
                Duration::from_secs(10),
                "expected Hello, got Some(Notify",
            ),
        ];
        for (peer, within, want) in cases {
            let temp = tempfile::tempdir().unwrap();
            let control = temp.path().join("c.sock");
            let has_peer = peer.is_some();
            let runner = peer
                .map(|peer| tokio::spawn(fake_runner(UnixListener::bind(&control).unwrap(), peer)));
            let mut checks = 0;
            let exited = || {
                checks += 1;
                (!has_peer && checks == 2).then(|| "exit status: 3".to_string())
            };
            let error = bootstrap(&control, within, exited).await.unwrap_err();
            assert!(error.starts_with(want), "{want}: {error}");
            if let Some(runner) = runner {
                runner.abort();
            }
        }
    }
}

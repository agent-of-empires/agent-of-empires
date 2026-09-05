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

/// Returns `Ok(())` if the structured view shim can be spawned (node on PATH, shim
/// file present, shim deps installed). Otherwise returns a short reason
/// that callers print before skipping. CI installs deps via `npm ci` in
/// `acp-worker/test-shim/` before running the integration leg; local
/// runs need the same one-shot setup, which the message points at.
pub fn shim_ready() -> Result<(), String> {
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err("node not on PATH".into());
    }
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
pub fn setup_temp_home() -> TempDir {
    let temp = TempDir::new().unwrap();
    set_temp_home(temp.path());
    temp
}

/// Variant for tests that already own a `TempDir` (e.g. ones that also seed
/// files under the same path before returning the guard).
pub fn set_temp_home(path: &Path) {
    // Establish the hermetic tmux socket before any lib tmux call so aoe's
    // once-cached socket resolution locks onto it (#2608).
    let _ = tmux_socket();
    std::env::set_var("HOME", path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
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
        "node",
        shim_path().to_str().unwrap(),
    ])
    .env("HOME", &home)
    .env("XDG_CONFIG_HOME", &xdg)
    .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
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

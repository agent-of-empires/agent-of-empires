//! Core e2e test harness built on tmux.
//!
//! `TuiTestHarness` launches `aoe` in a detached tmux session with an isolated
//! `$HOME`, sends keystrokes, captures screen output, and polls for expected
//! text. It also provides `run_cli` for exercising CLI subcommands as plain
//! subprocesses (no tmux).
//!
//! ## Recording
//!
//! Set `RECORD_E2E=1` to record each TUI test as an asciinema `.cast` file and
//! convert it to a GIF via `agg`. Recordings are saved to
//! `target/e2e-recordings/`. Both `asciinema` and `agg` must be on `$PATH`.

#[cfg(feature = "serve")]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// App dir name (mirrors `agent_of_empires::session::APP_DIR_NAME_*`).
// Debug builds use the `-dev` suffix; tests run in debug, so this resolves
// to `agent-of-empires-dev` for the binary under test.
// ---------------------------------------------------------------------------

/// Return the app dir under the given test home, matching `get_app_dir_path`.
pub fn app_dir_in(home: &Path) -> PathBuf {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        home.join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
    } else {
        home.join(agent_of_empires::session::APP_DIR_NAME_OTHER)
    }
}

// ---------------------------------------------------------------------------
// HOME isolation guard
// ---------------------------------------------------------------------------

/// RAII guard: points `HOME`/`XDG_CONFIG_HOME` at the harness's tempdir
/// for the test process and restores the prior values on `Drop`.
/// `#[serial]` on every caller linearizes this against other tests in
/// the binary; without the restore, a later test could inherit this
/// test's (by-then-dropped) tempdir path.
#[must_use = "HomeGuard restores env vars on Drop; bind it, don't discard it, or isolation ends immediately"]
pub struct HomeGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
}

impl HomeGuard {
    /// Snapshots the current `HOME`/`XDG_CONFIG_HOME` before overriding them,
    /// so `Drop` can restore the caller's real environment.
    pub fn new(home: &Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: env mutation; #[serial] linearizes this against every
        // other #[serial] test in the binary, so no concurrent
        // reader/writer exists.
        unsafe { std::env::set_var("HOME", home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
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

// ---------------------------------------------------------------------------
// tmux availability guard
// ---------------------------------------------------------------------------

pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip the calling test if tmux is not installed.
macro_rules! require_tmux {
    () => {
        if !$crate::harness::tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
    };
}
pub(crate) use require_tmux;

#[cfg(feature = "serve")]
pub fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip the calling test if Node.js is not installed. Acp e2e tests
/// drive the shared `web/tests/helpers/fakeAcpAgent.mjs` fake agent, which
/// is a Node script; without Node the worker can't speak ACP.
#[cfg(feature = "serve")]
macro_rules! require_node {
    () => {
        if !$crate::harness::node_available() {
            eprintln!("Skipping test: node not available");
            return;
        }
    };
}
#[cfg(feature = "serve")]
pub(crate) use require_node;

// ---------------------------------------------------------------------------
// Daemon port helpers (shared by serve.rs and structured view e2e)
// ---------------------------------------------------------------------------

/// Bind a TCP listener to an ephemeral port, drop it, and return the port.
/// Tiny TOCTOU window before the daemon binds, but acceptable for a serial
/// test.
#[cfg(feature = "serve")]
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// Poll until the daemon accepts a TCP connection on `port`. The parent
/// `aoe serve --daemon` returns as soon as it has spawned the child, so a
/// successful exit doesn't prove the child bound the port; this is the
/// real signal that the daemon is up.
#[cfg(feature = "serve")]
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
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

// ---------------------------------------------------------------------------
// Recording helpers
// ---------------------------------------------------------------------------

fn recording_enabled() -> bool {
    std::env::var("RECORD_E2E").is_ok_and(|v| v == "1" || v == "true")
}

fn asciinema_available() -> bool {
    Command::new("asciinema")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn agg_available() -> bool {
    Command::new("agg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn recordings_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/e2e-recordings");
    std::fs::create_dir_all(&dir).expect("create recordings dir");
    dir
}

fn convert_cast_to_gif(cast_path: &Path) {
    if !agg_available() {
        eprintln!(
            "agg not found -- skipping GIF conversion for {}",
            cast_path.display()
        );
        return;
    }

    let gif_path = cast_path.with_extension("gif");
    let status = Command::new("agg")
        .args(["--font-size", "14"])
        .arg(cast_path)
        .arg(&gif_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("Recorded GIF: {}", gif_path.display());
        }
        Ok(s) => {
            eprintln!("agg exited with {}, GIF not created", s);
        }
        Err(e) => {
            eprintln!("agg failed: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// TuiTestHarness
// ---------------------------------------------------------------------------

pub struct TuiTestHarness {
    session_name: String,
    test_name: String,
    home_dir: TempDir,
    _stub_dir: TempDir,
    binary_path: PathBuf,
    stub_path: PathBuf,
    socket_path: PathBuf,
    spawned: bool,
    recording: bool,
    cast_path: Option<PathBuf>,
    /// Extra env vars exported on every spawned process (tmux session +
    /// `run_cli` subprocesses). Used by structured view tests to thread
    /// FAKE_ACP_* and the runner-socket timeout into the daemon (and
    /// thus the daemon-spawned worker, which inherits this env).
    extra_env: Vec<(String, String)>,
    /// Dirs prepended to PATH ahead of the `claude` stub. Acp tests
    /// install the ACP shim here so it shadows the exit-0 stub.
    extra_path_dirs: Vec<PathBuf>,
    /// When set, `Drop` stops the structured view workers and the serve daemon
    /// before killing the tmux session, so a panicking assertion can't
    /// leak a daemon between serial tests.
    stop_daemon_on_drop: bool,
    /// When true, `install_acp_shim` bakes `FAKE_ACP_FORK_FAIL=1` into the shim
    /// so the fake agent rejects `session/fork`. Baked into the shim (not the
    /// daemon env) because the daemon `env_clear`s and allowlists env before
    /// spawning the worker, so a `set_env` knob would never reach the fake.
    acp_fork_fail: bool,
}

#[allow(dead_code)]
impl TuiTestHarness {
    /// Create a new harness with an isolated `$HOME` and a fake `claude` stub
    /// so tool detection succeeds.
    pub fn new(test_name: &str) -> Self {
        let home_dir = TempDir::new().expect("failed to create temp home");
        Self::with_home(test_name, home_dir)
    }

    /// Like [`new`](Self::new) but roots the isolated `$HOME` under `/tmp`.
    /// Acp workers bind a unix socket at
    /// `$HOME/.agent-of-empires-dev/acp-workers/<id>.sock`; a deep
    /// tempdir (macOS `/var/folders/...` is ~95 chars) blows past the
    /// 104-byte `sun_path` limit on Darwin, so the runner's
    /// `UnixListener::bind` fails. `/tmp` keeps the path short.
    #[cfg(unix)]
    pub fn new_in_tmp(test_name: &str) -> Self {
        let home_dir = TempDir::new_in("/tmp").expect("failed to create temp home under /tmp");
        Self::with_home(test_name, home_dir)
    }

    fn with_home(test_name: &str, home_dir: TempDir) -> Self {
        let stub_dir = TempDir::new().expect("failed to create stub dir");

        // Unique session name to avoid collisions.
        let session_name = format!("aoe_e2e_{}_{}", test_name, std::process::id());

        // Path to unique tmux socket for this test.
        let socket_path = home_dir.path().join("tmux.sock");

        // Create a fake `claude` script so `which claude` succeeds.
        let stub_path = stub_dir.path().to_path_buf();
        let claude_stub = stub_path.join("claude");
        std::fs::write(&claude_stub, "#!/bin/sh\nexit 0\n").expect("write claude stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&claude_stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod claude stub");
        }

        // Pre-seed config.toml to skip the welcome dialog and update checks.
        // `has_responded_to_telemetry` is set so the one-time telemetry consent
        // popup (gated on that flag alone in `App::new`) never renders over the
        // TUI and swallows input in the general e2e tests; the telemetry consent
        // surfaces are covered directly by their own unit and integration tests.
        // On Linux and macOS the app uses $XDG_CONFIG_HOME/agent-of-empires[-dev]/
        // (set below); other platforms use $HOME/.agent-of-empires[-dev]/. The
        // `-dev` suffix kicks in on debug builds, which is what `cargo test`
        // produces.
        let config_dir = app_dir_in(home_dir.path());
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let config_content = format!(
            r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{}"
"#,
            env!("CARGO_PKG_VERSION")
        );
        std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

        // Create default profile directory.
        std::fs::create_dir_all(config_dir.join("profiles").join("default"))
            .expect("create default profile dir");

        let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_aoe"));

        let recording = recording_enabled() && asciinema_available();
        if recording_enabled() && !asciinema_available() {
            eprintln!("RECORD_E2E is set but asciinema is not installed -- recording disabled");
        }

        let tmux_socket_env = socket_path.display().to_string();

        Self {
            session_name,
            test_name: test_name.to_string(),
            home_dir,
            _stub_dir: stub_dir,
            binary_path,
            stub_path,
            socket_path,
            spawned: false,
            recording,
            cast_path: None,
            // Pin the spawned aoe to the same tmux socket the harness drives
            // and inspects. aoe now routes every tmux call through an explicit
            // `-S <socket>` (#2608) instead of inheriting `$TMUX`, so without
            // this it would land on its own app-dir socket and the harness
            // would see none of its sessions.
            extra_env: vec![("AOE_TMUX_SOCKET".to_string(), tmux_socket_env)],
            extra_path_dirs: Vec::new(),
            stop_daemon_on_drop: false,
            acp_fork_fail: false,
        }
    }

    /// Build the PATH with structured view shim dirs (if any) and the stub
    /// directory prepended so the fake agent / fake `claude` is found.
    /// `extra_path_dirs` come first so an installed ACP shim shadows the
    /// exit-0 `claude` stub.
    fn env_path(&self) -> String {
        let system_path = std::env::var("PATH").unwrap_or_default();
        let mut parts: Vec<String> = self
            .extra_path_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        parts.push(self.stub_path.display().to_string());
        parts.push(system_path);
        parts.join(":")
    }

    /// Export an extra env var on every spawned process (tmux + `run_cli`).
    pub fn set_env(&mut self, key: &str, value: &str) {
        self.extra_env.push((key.to_string(), value.to_string()));
    }

    /// Make the fake ACP agent reject `session/fork` (for fork-failure tests).
    /// Must be called BEFORE `install_acp_shim` so the knob is baked into the
    /// shim: the daemon strips arbitrary env before spawning the worker, so a
    /// `set_env` knob would never reach the fake.
    pub fn set_acp_fork_fail(&mut self) {
        self.acp_fork_fail = true;
    }

    /// Install the shared Node fake-ACP agent as the `claude`,
    /// `claude-agent-acp`, and `aoe-agent` commands on PATH. The structured view
    /// supervisor resolves the `claude` tool key to the `claude-agent-acp`
    /// command via `AgentRegistry`, so all three names must point at the
    /// fake. `FAKE_ACP_SCRIPT` / `FAKE_ACP_DEBUG_LOG` are baked into the
    /// shim (the daemon -> runner -> node spawn chain does not reliably
    /// propagate process env). Also sets the runner-socket timeout high
    /// so a contended CI box doesn't trip the spawn deadline.
    pub fn install_acp_shim(&mut self, fake_acp_script: &Path) {
        let bin = self.home_dir.path().join("acp-bin");
        std::fs::create_dir_all(&bin).expect("create acp-bin dir");
        let fake_agent =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/tests/helpers/fakeAcpAgent.mjs");
        assert!(
            fake_agent.exists(),
            "fake ACP agent not found at {}",
            fake_agent.display()
        );
        let debug_log = app_dir_in(self.home_dir.path()).join("fake-acp.log");
        // Bake the fork-fail knob into the shim (not the daemon env) so it
        // survives the daemon's env_clear + allowlist when spawning the worker.
        let fork_fail_line = if self.acp_fork_fail {
            "export FAKE_ACP_FORK_FAIL=\"1\"\n"
        } else {
            ""
        };
        let script = format!(
            "#!/bin/sh\nexport FAKE_ACP_SCRIPT=\"{}\"\nexport FAKE_ACP_DEBUG_LOG=\"{}\"\n{}exec node \"{}\" \"$@\"\n",
            fake_acp_script.display(),
            debug_log.display(),
            fork_fail_line,
            fake_agent.display(),
        );
        for name in ["claude", "claude-agent-acp", "aoe-agent"] {
            let path = bin.join(name);
            std::fs::write(&path, &script).expect("write acp shim");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod acp shim");
            }
        }
        self.extra_path_dirs.push(bin);
        // Belt-and-suspenders: the shim bakes these in, but keep them on
        // the daemon env too for any path that bypasses the shim.
        self.set_env("FAKE_ACP_DEBUG_LOG", &debug_log.display().to_string());
        self.set_env("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS", "60000");
    }

    /// Install a no-op executable named `name` on the CLI PATH so a
    /// presence check (`which` / PATH scan) finds it. Used to stand in for
    /// an agent wrapper binary (e.g. an `agent_command_override` target)
    /// without installing the real tool. Returns the dir prepended to PATH.
    pub fn install_path_command(&mut self, name: &str) -> PathBuf {
        let bin = self.home_dir.path().join("path-bin");
        std::fs::create_dir_all(&bin).expect("create path-bin dir");
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write path command");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod path command");
        }
        self.extra_path_dirs.push(bin.clone());
        bin
    }

    /// Make `Drop` tear down structured view workers and the serve daemon.
    pub fn stop_daemon_on_drop(&mut self) {
        self.stop_daemon_on_drop = true;
    }

    /// Build the shell command string to run inside the tmux session.
    /// When recording, wraps the command with `asciinema rec`.
    fn build_tmux_command(&mut self, args: &[&str]) -> String {
        let mut aoe_cmd = self.binary_path.display().to_string();
        for arg in args {
            aoe_cmd.push(' ');
            aoe_cmd.push_str(arg);
        }

        if self.recording {
            let cast_path = recordings_dir().join(format!("{}.cast", self.test_name));
            let cmd = format!(
                "asciinema rec --overwrite --cols 100 --rows 30 -c '{}' {}",
                aoe_cmd,
                cast_path.display()
            );
            self.cast_path = Some(cast_path);
            cmd
        } else {
            aoe_cmd
        }
    }

    /// Spawn `aoe` (no arguments = TUI mode) inside a detached tmux session
    /// with a fixed 100x30 terminal.
    pub fn spawn_tui(&mut self) {
        self.spawn(&[]);
    }

    /// Spawn `aoe <args>` inside a detached tmux session.
    pub fn spawn(&mut self, args: &[&str]) {
        let cmd_str = self.build_tmux_command(args);

        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&self.session_name)
            .arg("-x")
            .arg("100")
            .arg("-y")
            .arg("30")
            .arg(&cmd_str)
            .env("HOME", self.home_dir.path())
            .env("XDG_CONFIG_HOME", self.home_dir.path().join(".config"))
            .env("PATH", self.env_path())
            .env("TERM", "xterm-256color")
            .envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .expect("failed to run tmux new-session");

        assert!(
            output.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        self.spawned = true;

        // Brief pause for the process to initialize.
        // Recording adds overhead so wait a bit longer.
        let delay = if self.recording { 500 } else { 300 };
        std::thread::sleep(Duration::from_millis(delay));
    }

    /// Send one or more tmux key names (e.g. "Enter", "Escape", "q", "C-c").
    pub fn send_keys(&self, keys: &str) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("send-keys")
            .arg("-t")
            .arg(&self.session_name)
            .arg(keys)
            .output()
            .expect("failed to send keys");
        assert!(
            output.status.success(),
            "send-keys failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Let the TUI process the keystroke.
        std::thread::sleep(Duration::from_millis(50));
    }

    /// Send a synthetic mouse event into the inner pane as an SGR
    /// escape sequence. crossterm's mouse capture (enabled by aoe at
    /// startup) parses the bytes the same way it would parse them
    /// from a real terminal, so click / scroll routing in the TUI
    /// runs the production code path. `button` is the SGR button code:
    /// 0 = left, 1 = middle, 2 = right; +32 = drag (rarely needed for
    /// click tests). `col` / `row` are 1-indexed terminal cells. Sends
    /// both the press (M) and release (m) so listeners that only fire
    /// on `Down(...)` (the click handlers) see a complete cycle.
    pub fn send_mouse_click(&self, button: u8, col: u16, row: u16) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        // XTerm SGR 1006 format: `CSI < Pb ; Px ; Py M` for press,
        // `... m` for release. No semicolon between Py and the final
        // M/m byte; crossterm parses the trailing-semicolon variant
        // leniently but spec-compliant terminals don't.
        let seq = format!("\x1b[<{button};{col};{row}M\x1b[<{button};{col};{row}m");
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("send-keys")
            .arg("-t")
            .arg(&self.session_name)
            .arg("-l")
            .arg(&seq)
            .output()
            .expect("failed to send mouse click");
        assert!(
            output.status.success(),
            "send_mouse_click failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    /// Send literal text (prevents "Enter" in text from being interpreted as
    /// the Enter key).
    pub fn type_text(&self, text: &str) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("send-keys")
            .arg("-t")
            .arg(&self.session_name)
            .arg("-l")
            .arg(text)
            .output()
            .expect("failed to type text");
        assert!(
            output.status.success(),
            "type_text failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    /// Capture the current screen contents as plain text (no ANSI escapes).
    pub fn capture_screen(&self) -> String {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("capture-pane")
            .arg("-t")
            .arg(&self.session_name)
            .arg("-p")
            .output()
            .expect("failed to capture pane");
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Poll `capture_screen()` until `text` appears. Panics with a screen dump
    /// if the default timeout (10s) is exceeded.
    pub fn wait_for(&self, text: &str) {
        self.wait_for_timeout(text, Duration::from_secs(10));
    }

    /// Like `wait_for` but with a custom timeout.
    pub fn wait_for_timeout(&self, text: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            let screen = self.capture_screen();
            if screen.contains(text) {
                return;
            }
            if start.elapsed() > timeout {
                panic!(
                    "Timed out waiting for {:?} after {:?}.\n\n--- Screen capture ---\n{}\n--- End screen capture ---",
                    text, timeout, screen
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Poll until `text` disappears from the screen.
    pub fn wait_for_absent(&self, text: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            let screen = self.capture_screen();
            if !screen.contains(text) {
                return;
            }
            if start.elapsed() > timeout {
                panic!(
                    "Timed out waiting for {:?} to disappear after {:?}.\n\n--- Screen capture ---\n{}\n--- End screen capture ---",
                    text, timeout, screen
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Assert that the screen currently contains `text`.
    /// Retries a few times to handle transient blank captures on macOS CI.
    pub fn assert_screen_contains(&self, text: &str) {
        let mut screen = String::new();
        for _ in 0..5 {
            screen = self.capture_screen();
            if screen.contains(text) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "Expected screen to contain {:?}.\n\n--- Screen capture ---\n{}\n--- End screen capture ---",
            text, screen
        );
    }

    /// Assert that the screen does NOT contain `text`.
    pub fn assert_screen_not_contains(&self, text: &str) {
        let screen = self.capture_screen();
        assert!(
            !screen.contains(text),
            "Expected screen NOT to contain {:?}.\n\n--- Screen capture ---\n{}\n--- End screen capture ---",
            text, screen
        );
    }

    /// Run `aoe <args>` as a subprocess (not in tmux) with the same env
    /// isolation. Returns the `Output` (stdout, stderr, status).
    ///
    /// Clears `AGENT_OF_EMPIRES_DEBUG` and `AOE_LOG_LEVEL` from the inherited
    /// env so tests run with a deterministic logging configuration. (aoe
    /// itself appends to `debug.log` now rather than truncating, but a
    /// child that opts in to file logging would still emit a marker line
    /// and an "aoe started" event under the test fixture, perturbing
    /// content-sensitive assertions.)
    pub fn run_cli(&self, args: &[&str]) -> Output {
        Command::new(&self.binary_path)
            .args(args)
            .env("HOME", self.home_dir.path())
            .env("XDG_CONFIG_HOME", self.home_dir.path().join(".config"))
            .env("PATH", self.env_path())
            .env_remove("AGENT_OF_EMPIRES_DEBUG")
            .env_remove("AOE_LOG_LEVEL")
            .envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .expect("failed to run aoe CLI")
    }

    /// Like [`Self::run_cli`], but writes `stdin` to the child before
    /// collecting output. Used by the plugin-worker tests, which speak
    /// ndjson JSON-RPC on stdio and exit on EOF.
    pub fn run_cli_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        let mut child = Command::new(&self.binary_path)
            .args(args)
            .env("HOME", self.home_dir.path())
            .env("XDG_CONFIG_HOME", self.home_dir.path().join(".config"))
            .env("PATH", self.env_path())
            .env_remove("AGENT_OF_EMPIRES_DEBUG")
            .env_remove("AOE_LOG_LEVEL")
            .envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn aoe CLI");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        // Dropping stdin closes the pipe; the worker exits on EOF.
        child.wait_with_output().expect("collect aoe CLI output")
    }

    /// Path to the isolated home directory for custom test setup.
    pub fn home_path(&self) -> &Path {
        self.home_dir.path()
    }

    /// Create and return a test project directory inside the temp home.
    pub fn project_path(&self) -> PathBuf {
        let p = self.home_dir.path().join("test-project");
        std::fs::create_dir_all(&p).expect("create project dir");
        p
    }

    /// Set `AOE_E2E_DEBUG=1` so the spawned TUI exports its
    /// watcher-config-refresh counter to
    /// `<app_dir>/.aoe_e2e_refresh_count` after every watcher-driven
    /// `apply_config_to_state`. Tests that poll the counter via
    /// `wait_for_watcher_config_refresh_above` must call this before
    /// `spawn_tui`; the env var is read by the TUI process.
    pub fn enable_e2e_debug_signals(&mut self) {
        self.set_env("AOE_E2E_DEBUG", "1");
    }

    /// Read the current watcher-config-refresh counter exported by the
    /// TUI. Returns 0 when the file is missing (TUI has not run any
    /// watcher refresh yet, or `AOE_E2E_DEBUG` was not set on the
    /// process).
    pub fn read_watcher_config_refresh_count(&self) -> u64 {
        let path = app_dir_in(self.home_dir.path()).join(".aoe_e2e_refresh_count");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| c.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Poll the watcher-config-refresh counter until it exceeds
    /// `baseline` or `timeout` elapses. Returns the new count on
    /// success and panics on timeout. Tests must take the baseline
    /// before triggering the config write so a subsequent
    /// watcher-driven refresh is the only way the counter climbs
    /// above it. Requires `enable_e2e_debug_signals` before
    /// `spawn_tui`.
    pub fn wait_for_watcher_config_refresh_above(&self, baseline: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        loop {
            let current = self.read_watcher_config_refresh_count();
            if current > baseline {
                return current;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {:?} waiting for watcher_config_refresh_count > {} (current = {}); \
                     check that enable_e2e_debug_signals() was called before spawn_tui and that the watcher \
                     subscription is wired",
                    timeout, baseline, current
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Check whether the tmux session is still alive.
    pub fn session_alive(&self) -> bool {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("has-session")
            .arg("-t")
            .arg(&self.session_name)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Wait until the tmux session terminates (the process exits).
    pub fn wait_for_exit(&self, timeout: Duration) {
        let start = Instant::now();
        loop {
            if !self.session_alive() {
                return;
            }
            if start.elapsed() > timeout {
                panic!(
                    "Timed out waiting for session {} to exit after {:?}",
                    self.session_name, timeout
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill_session(&self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket_path)
            .arg("kill-session")
            .arg("-t")
            .arg(&self.session_name)
            .output();
    }
}

impl Drop for TuiTestHarness {
    fn drop(&mut self) {
        // Stop structured view workers and the daemon before tearing down tmux so
        // a panicking assertion can't leak a daemon (which holds the test
        // port / pid file) into the next serial test. Worker first, then
        // daemon, so the fake-ACP child exits cleanly.
        if self.stop_daemon_on_drop {
            let _ = self.run_cli(&["acp", "stop", "--all"]);
            let _ = self.run_cli(&["serve", "--stop"]);
        }
        if self.spawned {
            self.kill_session();
        }

        // Convert recording to GIF if one was produced.
        if let Some(cast_path) = &self.cast_path {
            // Give asciinema a moment to finalize the file after the session ends.
            std::thread::sleep(Duration::from_millis(200));
            if cast_path.exists() {
                convert_cast_to_gif(cast_path);
            }
        }
    }
}

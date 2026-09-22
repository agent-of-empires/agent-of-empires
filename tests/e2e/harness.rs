//! tmux-driven e2e harness: `TuiTestHarness` runs `aoe` with an isolated
//! `$HOME` and tmux socket, sends keys, polls the screen, and runs CLI
//! subprocesses. `RECORD_E2E=1` records TUI tests to `target/e2e-recordings/`
//! (needs `asciinema` and `agg`).

use std::io::{Read, Seek, SeekFrom};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

pub fn app_dir_in(home: &Path) -> PathBuf {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        home.join(".config")
            .join(agent_of_empires::session::APP_DIR_NAME_XDG)
    } else {
        home.join(agent_of_empires::session::APP_DIR_NAME_OTHER)
    }
}

/// Points `HOME`/`XDG_CONFIG_HOME` at a test home for in-process library code
/// and restores them on `Drop`. Callers must be default-key `#[serial]` so no
/// `#[parallel]` test reads the env concurrently.
#[must_use = "HomeGuard restores env vars on Drop; bind it, don't discard it, or isolation ends immediately"]
pub struct HomeGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
}

impl HomeGuard {
    pub fn new(home: &Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: callers are default-key #[serial], so no concurrent env reader exists.
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
        fn restore_or_remove(key: &str, prev: Option<std::ffi::OsString>) {
            // SAFETY: same invariant as HomeGuard::new.
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

fn command_succeeds(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn tmux_available() -> bool {
    command_succeeds("tmux", "-V")
}

macro_rules! require_tmux {
    () => {
        if !$crate::harness::tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
    };
}
pub(crate) use require_tmux;

/// Resolve Node before the daemon drops host launcher state. PATH shims such as
/// Volta can recurse when invoked inside the worker's filtered environment.
fn node_executable() -> Option<PathBuf> {
    let output = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    path.is_file().then_some(path)
}

pub fn node_available() -> bool {
    node_executable().is_some()
}

/// Skip the calling test if Node.js (needed by the fake ACP agent) is missing.
macro_rules! require_node {
    () => {
        if !$crate::harness::node_available() {
            eprintln!("Skipping test: node not available");
            return;
        }
    };
}
pub(crate) use require_node;

/// Ephemeral port not yet issued to another test in this process. The
/// bind-then-drop TOCTOU window remains for unrelated processes.
pub fn pick_free_port() -> u16 {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static ISSUED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    let issued = ISSUED.get_or_init(|| Mutex::new(HashSet::new()));

    for _ in 0..64 {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            l.local_addr().expect("local_addr").port()
        };
        if issued.lock().expect("issued ports mutex").insert(port) {
            return port;
        }
    }
    panic!("could not find an unissued ephemeral port after 64 attempts");
}

/// `aoe serve --daemon` returns once the child is spawned, so poll the port.
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

/// Poll `probe` until it returns `Ok`; after `timeout`, panic with its last `Err`.
pub fn wait_until<T>(
    timeout: Duration,
    interval: Duration,
    mut probe: impl FnMut() -> Result<T, String>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let last = match probe() {
            Ok(value) => return value,
            Err(last) => last,
        };
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?}: {last}");
        }
        std::thread::sleep(interval);
    }
}

pub fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
    }
}

/// Create a git repo with one empty commit on `main`, isolated from user git config.
pub fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Parse the `ID: <id>` line `aoe add` prints on success.
pub fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

pub fn session_by_title<'a>(sessions: &'a Value, title: &str) -> &'a Value {
    sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .unwrap_or_else(|| panic!("no session titled '{title}' in sessions.json"))
}

pub fn agent_session_id_of(sessions: &Value, instance_id: &str) -> Option<String> {
    sessions
        .as_array()?
        .iter()
        .find(|r| r["id"].as_str() == Some(instance_id))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn recording_enabled() -> bool {
    std::env::var("RECORD_E2E").is_ok_and(|v| v == "1" || v == "true")
}

fn recordings_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/e2e-recordings");
    std::fs::create_dir_all(&dir).expect("create recordings dir");
    dir
}

fn convert_cast_to_gif(cast_path: &Path) {
    if !command_succeeds("agg", "--version") {
        eprintln!(
            "agg not found, skipping GIF conversion for {}",
            cast_path.display()
        );
        return;
    }
    let gif_path = cast_path.with_extension("gif");
    match Command::new("agg")
        .args(["--font-size", "14"])
        .arg(cast_path)
        .arg(&gif_path)
        .status()
    {
        Ok(s) if s.success() => eprintln!("Recorded GIF: {}", gif_path.display()),
        Ok(s) => eprintln!("agg exited with {}, GIF not created", s),
        Err(e) => eprintln!("agg failed: {}", e),
    }
}

pub struct TuiTestHarness {
    session_name: String,
    test_name: String,
    home_dir: TempDir,
    _stub_dir: TempDir,
    binary_path: PathBuf,
    stub_path: PathBuf,
    socket_path: PathBuf,
    spawned: bool,
    input_barrier: bool,
    render_log_offset: std::cell::Cell<u64>,
    recording: bool,
    cast_path: Option<PathBuf>,
    /// Exported on every spawned process (tmux session and `run_cli`).
    extra_env: Vec<(String, String)>,
    /// Prepended to PATH ahead of the `claude` stub.
    extra_path_dirs: Vec<PathBuf>,
    stop_daemon_on_drop: bool,
    acp_fork_fail: bool,
}

/// Raw ESC is a prefix byte: read together with the F12 fence, crossterm
/// decodes `ESC ESC [24~` as Esc plus literal `[24~` and the fence is lost.
/// The CSI-u encoding is self-delimiting and decodes to the same Esc press.
const ESCAPE_CSI_U: &[&str] = &["1b", "5b", "32", "37", "75"];

#[allow(dead_code)]
impl TuiTestHarness {
    /// Isolated `$HOME` with a fake `claude` stub so tool detection succeeds.
    pub fn new(test_name: &str) -> Self {
        let home_dir = TempDir::new().expect("failed to create temp home");
        Self::with_home(test_name, home_dir)
    }

    /// Roots `$HOME` under `/tmp` so ACP worker socket paths fit the 104-byte
    /// macOS `sun_path` limit.
    #[cfg(unix)]
    pub fn new_in_tmp(test_name: &str) -> Self {
        let home_dir = TempDir::new_in("/tmp").expect("failed to create temp home under /tmp");
        Self::with_home(test_name, home_dir)
    }

    /// `new_in_tmp` with the fake ACP agent running `script` (JSON) and
    /// worker/daemon teardown on drop.
    #[cfg(unix)]
    pub fn new_acp(test_name: &str, script: &str) -> Self {
        let mut h = Self::new_in_tmp(test_name);
        let script_path = h.home_path().join("fake-acp-script.json");
        std::fs::write(&script_path, script).expect("write fake-acp script");
        h.install_acp_shim(&script_path);
        h.stop_daemon_on_drop();
        h
    }

    fn with_home(test_name: &str, home_dir: TempDir) -> Self {
        let stub_dir = TempDir::new().expect("failed to create stub dir");
        let session_name = format!("aoe_e2e_{}_{}", test_name, std::process::id());
        let socket_path = home_dir.path().join("tmux.sock");
        let stub_path = stub_dir.path().to_path_buf();
        write_executable(&stub_path.join("claude"), "#!/bin/sh\nexit 0\n");

        // Skip the welcome, telemetry consent, and hooks dialogs plus update checks.
        let config_dir = app_dir_in(home_dir.path());
        std::fs::create_dir_all(config_dir.join("profiles").join("default"))
            .expect("create default profile dir");
        let config_content = format!(
            r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
has_acknowledged_agent_hooks = true
last_seen_version = "{}"
"#,
            env!("CARGO_PKG_VERSION")
        );
        std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

        let recording = recording_enabled() && command_succeeds("asciinema", "--version");
        if recording_enabled() && !recording {
            eprintln!("RECORD_E2E is set but asciinema is not installed, recording disabled");
        }
        let tmux_socket_env = socket_path.display().to_string();

        Self {
            session_name,
            test_name: test_name.to_string(),
            home_dir,
            _stub_dir: stub_dir,
            binary_path: PathBuf::from(env!("CARGO_BIN_EXE_aoe")),
            stub_path,
            socket_path,
            spawned: false,
            input_barrier: false,
            render_log_offset: std::cell::Cell::new(0),
            recording,
            cast_path: None,
            // aoe addresses tmux via `-S <socket>`, so pin it to the harness socket.
            extra_env: vec![("AOE_TMUX_SOCKET".to_string(), tmux_socket_env)],
            extra_path_dirs: Vec::new(),
            stop_daemon_on_drop: false,
            acp_fork_fail: false,
        }
    }

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

    /// `program` with the isolated home and PATH; `extra_env` is applied last
    /// so a test's `set_env` wins over `set` and `remove`.
    fn isolated(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        set: &[(&str, &str)],
        remove: &[&str],
    ) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", self.home_dir.path())
            .env("XDG_CONFIG_HOME", self.home_dir.path().join(".config"))
            .env("PATH", self.env_path())
            .envs(set.iter().copied());
        for key in remove {
            cmd.env_remove(key);
        }
        cmd.envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd
    }

    /// `tmux -S <harness socket>`.
    pub fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.arg("-S").arg(&self.socket_path);
        cmd
    }

    fn tmux_ok(&self, args: &[&str], what: &str) {
        let output = self.tmux().args(args).output().expect(what);
        assert!(
            output.status.success(),
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub fn tmux_has_session(&self, name: &str) -> bool {
        self.tmux()
            .args(["has-session", "-t", name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn tmux_kill_session(&self, name: &str) {
        let _ = self.tmux().args(["kill-session", "-t", name]).output();
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.extra_env.push((key.to_string(), value.to_string()));
    }

    pub fn add_path_dir(&mut self, dir: &Path) {
        self.extra_path_dirs.push(dir.to_path_buf());
    }

    /// Make the fake ACP agent reject `session/fork`. Call before
    /// `install_acp_shim`: the knob is baked into the shim because the daemon
    /// strips env before spawning the worker.
    pub fn set_acp_fork_fail(&mut self) {
        self.acp_fork_fail = true;
    }

    /// Install the Node fake ACP agent as `claude`, `claude-agent-acp`, and
    /// `aoe-agent`. Its env is baked into the shim because the daemon, runner,
    /// and node spawn chain does not propagate process env.
    pub fn install_acp_shim(&mut self, fake_acp_script: &Path) {
        self.install_acp_shim_inner(fake_acp_script, None);
    }

    /// Like `install_acp_shim`, also recording each adapter invocation's env to
    /// `capture_dir/<pid>`, after daemon-side filtering.
    pub fn install_acp_shim_capturing_env(&mut self, fake_acp_script: &Path, capture_dir: &Path) {
        self.install_acp_shim_inner(fake_acp_script, Some(capture_dir));
    }

    fn install_acp_shim_inner(&mut self, fake_acp_script: &Path, capture_dir: Option<&Path>) {
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
        let node = node_executable().expect("resolve Node.js executable");
        let fork_fail_line = if self.acp_fork_fail {
            "export FAKE_ACP_FORK_FAIL=\"1\"\n"
        } else {
            ""
        };
        let capture_line = capture_dir
            .map(|dir| {
                let dir = dir.display();
                format!("mkdir -p \"{dir}\"\nenv | sort > \"{dir}/$$\"\n")
            })
            .unwrap_or_default();
        let script = format!(
            "#!/bin/sh\nexport FAKE_ACP_SCRIPT=\"{}\"\nexport FAKE_ACP_DEBUG_LOG=\"{}\"\n{}{}exec \"{}\" \"{}\" \"$@\"\n",
            fake_acp_script.display(),
            debug_log.display(),
            fork_fail_line,
            capture_line,
            node.display(),
            fake_agent.display(),
        );
        for name in ["claude", "claude-agent-acp", "aoe-agent"] {
            write_executable(&bin.join(name), &script);
        }
        self.extra_path_dirs.push(bin);
        self.set_env("FAKE_ACP_DEBUG_LOG", &debug_log.display().to_string());
        self.set_env("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS", "60000");
    }

    /// Install a no-op `name` on PATH; returns the dir it lives in.
    pub fn install_path_command(&mut self, name: &str) -> PathBuf {
        let bin = self.home_dir.path().join("path-bin");
        std::fs::create_dir_all(&bin).expect("create path-bin dir");
        write_executable(&bin.join(name), "#!/bin/sh\nexit 0\n");
        self.extra_path_dirs.push(bin.clone());
        bin
    }

    /// Install a PATH stub that writes its argv to the returned file. Launches
    /// run through an env-file wrapper, so this is where the real command shows.
    pub fn install_recording_path_command(&mut self, name: &str) -> PathBuf {
        let bin = self.home_dir.path().join("path-bin");
        std::fs::create_dir_all(&bin).expect("create path-bin dir");
        let record = self.home_dir.path().join(format!("{name}.argv"));
        // The pane does not inherit harness env, so embed the absolute path.
        let record_str = record.to_string_lossy();
        assert!(
            !record_str.contains(['"', '$', '`', '\\']),
            "record path has shell metacharacters: {record_str}"
        );
        write_executable(
            &bin.join(name),
            &format!("#!/bin/sh\nprintf '%s ' \"$0\" \"$@\" > \"{record_str}\"\nexit 0\n"),
        );
        self.extra_path_dirs.push(bin);
        record
    }

    pub fn stop_daemon_on_drop(&mut self) {
        self.stop_daemon_on_drop = true;
    }

    /// Start `aoe serve --daemon --no-auth` on a free port and wait for it to bind.
    pub fn start_daemon(&self) -> u16 {
        self.start_daemon_with(&["--no-auth"])
    }

    /// `start_daemon` with different auth or proxy flags.
    pub fn start_daemon_with(&self, args: &[&str]) -> u16 {
        let port = pick_free_port();
        let port_s = port.to_string();
        self.run_cli_ok(&[&["serve", "--daemon", "--port", &port_s], args].concat());
        assert!(
            wait_for_port(port, Duration::from_secs(10)),
            "daemon never bound port {port}"
        );
        port
    }

    /// Run `aoe add <args>`, assert success, and return the new session id.
    pub fn add_session(&self, args: &[&str]) -> String {
        parse_session_id(&self.run_cli_ok(&[&["add"], args].concat()))
    }

    /// Start a daemon and add a `claude --structured-view` session titled
    /// `title` over a fresh git project; returns the daemon port and session id.
    pub fn start_structured_session(&self, title: &str) -> (u16, String) {
        let project = self.project_path();
        init_git_repo(&project);
        let port = self.start_daemon();
        let id = self.add_session(&[
            project.to_str().unwrap(),
            "-t",
            title,
            "-c",
            "claude",
            "--structured-view",
        ]);
        (port, id)
    }

    /// Retry `aoe acp prompt` until accepted; the prompt fails while the
    /// worker is still spawning or handshaking.
    pub fn prompt_until_accepted(&self, session_id: &str, text: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let out = self.run_cli(&["acp", "prompt", session_id, text]);
            if out.status.success() {
                return;
            }
            if Instant::now() >= deadline {
                let ps = self.run_cli(&["ps", "--acp", "--dead", "--json"]);
                panic!(
                    "structured view worker never accepted a prompt within {timeout:?}.\n\
                     last prompt stdout: {}\n last prompt stderr: {}\n ps --acp: {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                    String::from_utf8_lossy(&ps.stdout),
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Append TOML to the seeded `config.toml`.
    pub fn append_config(&self, toml: &str) {
        let path = app_dir_in(self.home_path()).join("config.toml");
        let seeded = std::fs::read_to_string(&path).expect("read seeded config");
        std::fs::write(&path, format!("{seeded}\n{toml}\n")).expect("write config.toml");
    }

    pub fn sessions_path(&self) -> PathBuf {
        app_dir_in(self.home_path()).join("profiles/default/sessions.json")
    }

    pub fn read_sessions(&self) -> Value {
        let path = self.sessions_path();
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        serde_json::from_str(&content).expect("invalid sessions JSON")
    }

    /// `read_sessions` that yields `Null` for a missing or mid-write file.
    pub fn try_read_sessions(&self) -> Value {
        let content = std::fs::read_to_string(self.sessions_path()).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(Value::Null)
    }

    fn build_tmux_command(&mut self, args: &[&str]) -> String {
        let mut aoe_cmd = self.binary_path.display().to_string();
        if self.input_barrier {
            let path = self.home_dir.path().join("input-barrier");
            aoe_cmd = format!(
                "env -u NO_COLOR AOE_E2E_INPUT_BARRIER={} {aoe_cmd}",
                shell_words::quote(path.to_str().expect("input barrier path"))
            );
        }
        for arg in args {
            aoe_cmd.push(' ');
            aoe_cmd.push_str(arg);
        }

        if !self.recording {
            return aoe_cmd;
        }
        let cast_path = recordings_dir().join(format!("{}.cast", self.test_name));
        let cmd = format!(
            "asciinema rec --overwrite --cols 100 --rows 30 -c {} {}",
            shell_words::quote(&aoe_cmd),
            shell_words::quote(cast_path.to_str().expect("recording path"))
        );
        self.cast_path = Some(cast_path);
        cmd
    }

    pub fn spawn_tui(&mut self) {
        self.spawn(&[]);
    }

    /// Spawn `aoe <args>` in a detached 100x30 tmux session.
    pub fn spawn(&mut self, args: &[&str]) {
        self.input_barrier = args.first() != Some(&"add");
        let cmd_str = self.build_tmux_command(args);
        let start_gate = self.home_dir.path().join("tui-start");
        let cmd_str = if self.input_barrier {
            if start_gate.exists() {
                std::fs::remove_file(&start_gate).expect("reset TUI startup gate");
            }
            self.render_log_offset.set(0);
            std::fs::File::create(self.home_dir.path().join("render.log"))
                .expect("create render observation");
            format!(
                "while [ ! -e {} ]; do sleep 0.01; done; {cmd_str}",
                shell_words::quote(start_gate.to_str().expect("start gate path"))
            )
        } else {
            cmd_str
        };
        self.new_tmux_session(&self.session_name, "100", "30", &cmd_str);
        self.spawned = true;

        if self.input_barrier {
            let path = self.home_dir.path().join("render.log");
            let pipe_cmd = format!(
                "cat >> {}",
                shell_words::quote(path.to_str().expect("render log path"))
            );
            self.tmux_ok(
                &["pipe-pane", "-O", "-t", &self.session_name, &pipe_cmd],
                "pipe-pane",
            );
            std::fs::write(start_gate, b"start").expect("release observed TUI startup");
            self.wait_for_input_ack(0, Duration::from_secs(30));
        }
    }

    fn new_tmux_session(&self, name: &str, cols: &str, rows: &str, cmd: &str) {
        let output = self
            .isolated("tmux", &[("TERM", "xterm-256color")], &["NO_COLOR"])
            .arg("-S")
            .arg(&self.socket_path)
            .args(["new-session", "-d", "-s", name, "-x", cols, "-y", rows, cmd])
            .output()
            .expect("failed to run tmux new-session");
        assert!(
            output.status.success(),
            "tmux new-session failed for {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Create a detached tmux session on the harness socket with the harness
    /// env. A tmux server's env is fixed by its first client, so a bare `tmux`
    /// pre-create would pin the real `$HOME` and socket onto the server.
    pub fn tmux_new_detached(&self, name: &str, cmd: &str) {
        self.new_tmux_session(name, "80", "24", cmd);
    }

    /// Send tmux key names (e.g. "Enter", "Escape", "q", "C-c").
    pub fn send_keys(&self, keys: &str) {
        if matches!(keys, "Escape" | "C-[") {
            self.send_hex_keys(ESCAPE_CSI_U);
        } else {
            self.send_keys_unfenced(keys);
        }
        self.synchronize_input();
    }

    fn send_hex_keys<S: AsRef<str>>(&self, bytes: &[S]) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let mut args = vec!["send-keys", "-t", &self.session_name, "-H"];
        args.extend(bytes.iter().map(AsRef::as_ref));
        self.tmux_ok(&args, "send-keys");
    }

    /// Native terminal owners cannot receive an outer-TUI F12 fence.
    /// Observe their lifecycle before resuming ordinary TUI input.
    pub fn send_keys_unfenced(&self, keys: &str) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        self.tmux_ok(&["send-keys", "-t", &self.session_name, keys], "send-keys");
    }

    pub fn terminal_resume_sequence(&self) -> u64 {
        match std::fs::read_to_string(self.home_dir.path().join("input-barrier.resumed")) {
            Ok(value) => value.parse().expect("terminal resume sequence"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("read terminal resume sequence: {error}"),
        }
    }

    pub fn wait_for_terminal_resume(&self, previous: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let resumed =
                std::fs::read_to_string(self.home_dir.path().join("input-barrier.resumed"))
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok());
            if resumed.is_some_and(|sequence| sequence > previous) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "outer TUI did not regain terminal ownership"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.synchronize_input();
    }

    /// Send an SGR 1006 press and release. `button`: 0 left, 1 middle, 2 right;
    /// `col`/`row` are 1-indexed cells.
    pub fn send_mouse_click(&self, button: u8, col: u16, row: u16) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let seq = format!("\x1b[<{button};{col};{row}M\x1b[<{button};{col};{row}m");
        self.tmux_ok(
            &["send-keys", "-t", &self.session_name, "-l", &seq],
            "send_mouse_click",
        );
        self.synchronize_input();
    }

    /// Deliver `text` as one bracketed paste, hex-encoded so tmux cannot
    /// reinterpret newlines or semicolons.
    pub fn send_paste(&self, text: &str) {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.send_hex_keys(&hex);
        self.synchronize_input();
    }

    /// Send literal text (so "Enter" in text is not the Enter key).
    pub fn type_text(&self, text: &str) {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        self.tmux_ok(
            &["send-keys", "-t", &self.session_name, "-l", text],
            "type_text",
        );
        self.synchronize_input();
    }

    fn synchronize_input(&self) {
        if !self.input_barrier || !self.session_alive() {
            return;
        }
        let current = self.input_sequence();
        let output = self
            .tmux()
            .args(["send-keys", "-t", &self.session_name, "F12"])
            .output()
            .expect("send input barrier");
        if !output.status.success() && !self.session_alive() {
            return;
        }
        assert!(output.status.success(), "input barrier send failed");
        self.wait_for_input_ack(current + 1, Duration::from_secs(10));
    }

    fn input_sequence(&self) -> u64 {
        std::fs::read_to_string(self.home_dir.path().join("input-barrier"))
            .expect("TUI startup acknowledged")
            .parse()
            .expect("input sequence")
    }

    fn wait_for_input_ack(&self, sequence: u64, timeout: Duration) {
        let mut log = std::fs::File::open(self.home_dir.path().join("render.log"))
            .expect("open terminal observation");
        log.seek(SeekFrom::Start(self.render_log_offset.get()))
            .expect("seek terminal observation");
        let token = format!("\x1b]0;aoe-e2e-{sequence}\x07");
        let mut observed = Vec::with_capacity(8192 + token.len());
        let mut chunk = [0u8; 8192];
        let deadline = Instant::now() + timeout;
        loop {
            assert!(
                Instant::now() < deadline,
                "TUI did not render input sequence {sequence}"
            );
            let count = log.read(&mut chunk).expect("read terminal observation");
            if count > 0 {
                self.render_log_offset
                    .set(log.stream_position().expect("terminal observation offset"));
                observed.extend_from_slice(&chunk[..count]);
                if observed
                    .windows(token.len())
                    .any(|window| window == token.as_bytes())
                {
                    // tmux queues pipe output before parsing, in one callback.
                    // A subsequent server command witnesses that callback completing.
                    let parsed = self
                        .tmux()
                        .args([
                            "display-message",
                            "-p",
                            "-t",
                            &self.session_name,
                            "#{pane_id}",
                        ])
                        .output()
                        .expect("complete terminal parser barrier");
                    assert!(
                        parsed.status.success(),
                        "terminal disappeared before render acknowledgment"
                    );
                    return;
                }
                let discard = observed.len().saturating_sub(token.len() - 1);
                observed.drain(..discard);
                continue;
            }
            if sequence > 0 && !self.session_alive() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not render input sequence {sequence}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn capture_screen(&self) -> String {
        self.capture_pane(false)
    }

    /// `capture_screen` keeping escape sequences, for styling assertions.
    pub fn capture_screen_styled(&self) -> String {
        self.capture_pane(true)
    }

    /// tmux re-emits OSC 8 hyperlinks through `capture-pane -e` from 3.4.
    pub fn tmux_reemits_hyperlinks() -> bool {
        let Ok(out) = Command::new("tmux").arg("-V").output() else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let version = String::from_utf8_lossy(&out.stdout);
        let digits: String = version
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = digits.split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor) >= (3, 4)
    }

    fn capture_pane(&self, styled: bool) -> String {
        assert!(self.spawned, "must call spawn_tui() or spawn() first");
        let mut cmd = self.tmux();
        cmd.args(["capture-pane", "-t", &self.session_name, "-p"]);
        if styled {
            cmd.arg("-e");
        }
        let output = cmd.output().expect("failed to capture pane");
        assert!(
            output.status.success(),
            "capture-pane failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    pub fn wait_for(&self, text: &str) {
        self.wait_for_timeout(text, Duration::from_secs(10));
    }

    /// Wait for the home banner. First run migrates from `v0` behind a spinner,
    /// which can outlast the default `wait_for` budget on loaded CI.
    pub fn wait_for_ready(&self) {
        self.wait_for_timeout(" aoe ", Duration::from_secs(30));
    }

    pub fn wait_for_timeout(&self, text: &str, timeout: Duration) {
        self.wait_for_screen(text, true, timeout);
    }

    pub fn wait_for_absent(&self, text: &str, timeout: Duration) {
        self.wait_for_screen(text, false, timeout);
    }

    fn wait_for_screen(&self, text: &str, present: bool, timeout: Duration) {
        let start = Instant::now();
        loop {
            let screen = self.capture_screen();
            if screen.contains(text) == present {
                return;
            }
            if start.elapsed() > timeout {
                let what = if present { "" } else { " to disappear" };
                panic!(
                    "Timed out waiting for {text:?}{what} after {timeout:?}.\n\n--- Screen capture ---\n{screen}\n--- End screen capture ---",
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Retries to ride out transient blank captures on macOS CI.
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

    pub fn assert_screen_not_contains(&self, text: &str) {
        let screen = self.capture_screen();
        assert!(
            !screen.contains(text),
            "Expected screen NOT to contain {:?}.\n\n--- Screen capture ---\n{}\n--- End screen capture ---",
            text, screen
        );
    }

    fn cli_command(&self, args: &[&str]) -> Command {
        let mut cmd = self.isolated(
            &self.binary_path,
            &[],
            &["AGENT_OF_EMPIRES_DEBUG", "AOE_LOG_LEVEL"],
        );
        cmd.args(args);
        cmd
    }

    /// Run `aoe <args>` as a plain subprocess with the harness env and
    /// deterministic logging.
    pub fn run_cli(&self, args: &[&str]) -> Output {
        self.cli_command(args)
            .output()
            .expect("failed to run aoe CLI")
    }

    /// `run_cli`, asserting success and returning stdout.
    pub fn run_cli_ok(&self, args: &[&str]) -> String {
        let out = self.run_cli(args);
        assert!(
            out.status.success(),
            "aoe {args:?} failed.\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `run_cli`, asserting a non-zero exit and returning stderr.
    pub fn run_cli_err(&self, args: &[&str]) -> String {
        let out = self.run_cli(args);
        assert!(
            !out.status.success(),
            "aoe {args:?} unexpectedly succeeded.\nstdout: {}",
            String::from_utf8_lossy(&out.stdout),
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    pub fn run_cli_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = self
            .cli_command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn aoe CLI");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("collect aoe CLI output")
    }

    pub fn home_path(&self) -> &Path {
        self.home_dir.path()
    }

    pub fn project_path(&self) -> PathBuf {
        let p = self.home_dir.path().join("test-project");
        std::fs::create_dir_all(&p).expect("create project dir");
        p
    }

    /// Make the TUI export its watcher config refresh count; call before `spawn_tui`.
    pub fn enable_e2e_debug_signals(&mut self) {
        self.set_env("AOE_E2E_DEBUG", "1");
    }

    pub fn read_watcher_config_refresh_count(&self) -> u64 {
        let path = app_dir_in(self.home_dir.path()).join(".aoe_e2e_refresh_count");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| c.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Poll until the refresh count exceeds `baseline`, taken before the config write.
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

    pub fn session_alive(&self) -> bool {
        self.tmux_has_session(&self.session_name)
    }

    pub fn wait_for_exit(&self, timeout: Duration) {
        let start = Instant::now();
        while self.session_alive() {
            if start.elapsed() > timeout {
                panic!(
                    "Timed out waiting for session {} to exit after {:?}",
                    self.session_name, timeout
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for TuiTestHarness {
    fn drop(&mut self) {
        // Workers before the daemon, so the fake ACP child exits cleanly.
        if self.stop_daemon_on_drop {
            let _ = self.run_cli(&["acp", "stop", "--all"]);
            let _ = self.run_cli(&["serve", "--stop"]);
        }
        // The socket is per test; killing the server reaps every session on it.
        let _ = self.tmux().arg("kill-server").output();

        if let Some(cast_path) = &self.cast_path {
            // Give asciinema a moment to finalize the file after the session ends.
            std::thread::sleep(Duration::from_millis(200));
            if cast_path.exists() {
                convert_cast_to_gif(cast_path);
            }
        }
    }
}

#[cfg(unix)]
#[test]
#[serial_test::parallel]
fn tui_input_barrier_survives_escape_in_same_read() {
    require_tmux!();
    let mut harness = TuiTestHarness::new("escape_fence");
    harness.spawn_tui();
    harness.wait_for("No sessions yet");
    harness.send_keys("n");
    harness.wait_for("Title");

    // One send-keys is one write, so the TUI reads Escape and the fence together.
    let f12 = ["1b", "5b", "32", "34", "7e"];
    let sequence = harness.input_sequence() + 1;
    harness.send_hex_keys(&[ESCAPE_CSI_U, &f12].concat());
    harness.wait_for_input_ack(sequence, Duration::from_secs(10));
    assert!(!harness.capture_screen().contains("Title"));
}

#[cfg(unix)]
#[test]
#[serial_test::parallel]
fn tui_input_barrier_handles_shell_metacharacters_in_home() {
    require_tmux!();
    let home = tempfile::Builder::new()
        .prefix("aoe ' $ ` ")
        .tempdir_in("/tmp")
        .unwrap();
    let mut harness = TuiTestHarness::with_home("quoted_home", home);
    harness.spawn_tui();
    harness.wait_for("No sessions yet");
    harness.send_keys("q");
    harness.wait_for("Quit Agent of Empires");
    harness.send_keys("y");
    harness.wait_for_exit(Duration::from_secs(5));
}

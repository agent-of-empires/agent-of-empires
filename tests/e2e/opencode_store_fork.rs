//! A store fork of an OpenCode 2.x conversation happens before the pane exists.
//!
//! OpenCode 2.x rejects the root `--fork` flag AoE used to emit, so the fork is
//! requested from the store over `POST /api/session/{id}/fork` against a
//! short-lived `opencode serve`. Nothing below unit level exercises that
//! composed path: the serve child is spawned by the AoE binary with its own
//! environment and reaped once the fork returns, and the id it mints has to
//! reach the launch command line.
//!
//! The fake agent is a shell script because it has to answer on the port AoE
//! chooses at spawn time, and that listener is a separate Python program so the
//! script stays free of nested quoting.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

const PARENT: &str = "OpencodeForkParent";
const CHILD: &str = "OpencodeForkChild";
const PARENT_ID: &str = "ses_11111111111111111111111111aaaa";
const CHILD_ID: &str = "ses_22222222222222222222222222bbbb";

/// The JSON body the store answers both routes with. The store mints the child,
/// and AoE reads it back out of `data.id`. It carries no line breaks, so it can
/// be handed to the listener as an argument.
fn child_body() -> String {
    format!(r#"{{"data":{{"id":"{CHILD_ID}"}}}}"#)
}

/// A loopback listener that answers every request and stays bound, which is what
/// a real `opencode serve` does. Written to disk rather than inlined so no
/// quoting has to survive the shell.
fn server_program() -> String {
    [
        "import socket, sys",
        "port = int(sys.argv[1])",
        "body = sys.argv[2].encode()",
        "srv = socket.socket()",
        "srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)",
        "srv.bind((\"127.0.0.1\", port))",
        "srv.listen(16)",
        "sys.stderr.write('BOUND on %d\\n' % port); sys.stderr.flush()",
        "response = (b\"HTTP/1.1 200 OK\\r\\n\"",
        "            b\"Content-Type: application/json\\r\\n\"",
        "            + (\"Content-Length: %d\\r\\n\\r\\n\" % len(body)).encode()",
        "            + body + b\"\\r\\n\")",
        "while True:",
        "    conn, _ = srv.accept()",
        "    try:",
        "        conn.recv(65535)",
        "        conn.sendall(response)",
        "    finally:",
        "        conn.close()",
        "",
    ]
    .join("\n")
}

/// The fake agent. `listener` is the path of the server program to run in the
/// `serve` branch; without it the agent answers `--help` and nothing else, which
/// is what the refusal case needs.
fn fake_script(
    log: &Path,
    launch_log: &Path,
    listener: Option<&Path>,
    body: &str,
    python: &str,
) -> String {
    let mut s = String::from("#!/bin/sh\n");
    s.push_str(&format!(
        "log='{}'\nlaunch='{}'\n",
        log.display(),
        launch_log.display()
    ));
    s.push_str("printf '%s\\n' \"$*\" >> \"$log\"\n");
    // The pane runs through the harness env-file wrapper, so the launch line is
    // recorded here rather than inferred from the argv log above. Overwritten
    // each call, so the last line is the launch that survived.
    s.push_str("printf '%s ' \"$0\" \"$@\" > \"$launch\"\n");
    s.push_str("if [ \"$1\" = \"--help\" ]; then\n");
    s.push_str("  printf 'FLAGS\\n  --auto  approve\\n  --session, -s string  Session ID\\n'\n  exit 0\nfi\n");
    if let Some(listener) = listener {
        s.push_str("if [ \"$1\" = \"serve\" ]; then\n");
        // AoE names the endpoint with `--port N`; nothing else carries it.
        s.push_str("  port=''; while [ $# -gt 0 ]; do\n    if [ \"$1\" = \"--port\" ]; then port=\"$2\"; fi\n    shift\n  done\n");
        s.push_str("  [ -z \"$port\" ] && exit 1\n");
        s.push_str(&format!(
            "  exec '{}' '{}' \"$port\" '{}' >> \"$log.err\" 2>&1\n",
            python,
            listener.display(),
            body
        ));
        s.push_str("fi\n");
    }
    s.push_str("exec sleep 30\n");
    s
}

/// The serve child runs with a cleared environment rebuilt from the session
/// config, so the interpreter is named by absolute path rather than found
/// through a `PATH` the launch does not pass on.
fn interpreter() -> String {
    which::which("python3")
        .expect("python3 is required to host the fake store server")
        .display()
        .to_string()
}

fn install_fake_opencode(h: &mut TuiTestHarness, serve: bool) -> PathBuf {
    let bin = h.install_path_command("opencode");
    let log = h.home_path().join("fake-opencode.log");
    let listener = if serve {
        let path = h.home_path().join("fake-opencode-server.py");
        fs::write(&path, server_program()).expect("write fake server");
        Some(path)
    } else {
        None
    };
    let launch_log = h.home_path().join("fake-opencode-launch.log");
    let script = fake_script(
        &log,
        &launch_log,
        listener.as_deref(),
        &child_body(),
        &interpreter(),
    );
    let path = bin.join("opencode");
    fs::write(&path, script).expect("write fake opencode");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake opencode");
    }
    log
}

/// Reads `path` until a line starting with `want` appears. Polling observes a
/// file another process writes; the deadline bounds the wait, it does not stand
/// in for one. Matching a specific line matters: the generation probe writes
/// before the fork spawns the server.
/// Waits for a file the stub rewrites in place, and returns its last line.
fn wait_for_line(path: &Path, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let contents = fs::read_to_string(path).unwrap_or_default();
        // The stub rewrites this file on every call, so a launch is whatever is
        // there once the ephemeral server has been reaped and stopped writing.
        let last = contents.trim();
        if !last.is_empty() && !last.contains("serve") && last != "--help" {
            return last.to_string();
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for(path: &Path, want: &str, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let contents = fs::read_to_string(path).unwrap_or_default();
        if contents.lines().any(|line| line.starts_with(want)) {
            return contents;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct StopSessionsOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionsOnDrop<'_> {
    fn drop(&mut self) {
        for title in [CHILD, PARENT] {
            let _ = self.h.run_cli(&["session", "stop", title]);
        }
    }
}

/// OpenCode will not attest a store unless the database is pinned, so the test
/// declares one and seeds the parent conversation in it. The fake agent never
/// reads it; AoE resolves the routing against it.
fn seed_store(h: &TuiTestHarness) -> String {
    // AoE compares canonical paths and the harness project sits under a
    // temporary directory that may carry a symlink.
    let project = fs::canonicalize(h.project_path()).expect("canonicalize project");
    let data = h.home_path().join("opencode-data");
    fs::create_dir_all(&data).expect("create opencode data dir");
    let database = data.join("opencode.db");
    let connection = rusqlite::Connection::open(&database).expect("create opencode store");
    let schema = "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, \
         workspace_id TEXT, parent_id TEXT, fork_session_id TEXT, fork_boundary TEXT, \
         slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT, \
         version TEXT NOT NULL, share_url TEXT, metadata TEXT, cost REAL, agent TEXT, \
         model TEXT, time_created INTEGER, time_updated INTEGER)";
    let seed = format!(
        "INSERT INTO session_v2 (id, project_id, directory, slug, version, time_created, time_updated)\
         VALUES ('{PARENT_ID}', 'p1', '{}', 'p', '2.0.22', 0, 0)",
        project.display()
    );
    connection
        .execute_batch(&format!("{schema}; {seed}"))
        .expect("seed the parent conversation");
    database.display().to_string()
}

/// The store mints the child, the launch opens that child, and the flag 2.x
/// refuses never reaches the command line.
#[test]
#[parallel]
fn opencode_store_fork_opens_the_child_the_store_minted() {
    require_tmux!();

    let mut h = TuiTestHarness::new("opencode_store_fork");
    let database = seed_store(&h);
    h.set_env("OPENCODE_DB", &database);
    let log_path = install_fake_opencode(&mut h, true);
    let launch_log = h.home_path().join("fake-opencode-launch.log");
    let project = h.project_path();

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "opencode",
        "-t",
        PARENT,
    ]);
    h.run_cli_ok(&["session", "set-session-id", PARENT, PARENT_ID]);
    let _cleanup = StopSessionsOnDrop { h: &h };

    // A fork is created by the command, not by starting a session that already
    // holds a conversation id.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "opencode",
        "-t",
        CHILD,
        "--fork-from",
        PARENT,
        "--launch",
    ]);
    let stderr = String::from_utf8_lossy(&add.stderr);
    let logged = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        add.status.success(),
        "aoe add --fork-from --launch failed:\n{stderr}\nagent argv log:\n{logged}"
    );

    let invocations = wait_for(&log_path, "serve", "the ephemeral server spawn");
    let serve = invocations
        .lines()
        .find(|line| line.starts_with("serve"))
        .expect("a serve invocation was logged");
    assert!(
        serve.contains("--port"),
        "the server must be asked for the endpoint it serves on; argv: {serve:?}"
    );
    // The generation probe also invokes the binary, so the launch is the line
    // that is neither the probe nor the ephemeral server.
    // The stub is overwritten on every call, so the surviving line is the TUI
    // launch. Waiting for it distinguishes a launch from the probe and the
    // ephemeral server, which also pass through this stub.
    // The server stub keeps rewriting the launch file, so the launch is the line
    // that survives after it has been reaped.
    let launch = wait_for_line(&launch_log, "the launch command line");
    assert!(
        !launch.contains("--fork"),
        "OpenCode 2.x rejects the root --fork flag; launch argv: {launch:?}"
    );

    assert!(
        !launch.contains("--fork"),
        "OpenCode 2.x rejects the root --fork flag; launch argv: {launch:?}"
    );
    assert!(
        !launch.contains(PARENT_ID),
        "the fork must not reopen the parent; launch argv: {launch:?}"
    );
    assert!(
        launch.contains(CHILD_ID),
        "the launch must open the child the store minted; launch argv: {launch:?}"
    );
}

/// With no store reachable the fork must be refused rather than started
/// unforked, and the refusal must say so.
#[test]
#[parallel]
fn opencode_store_fork_refuses_rather_than_starting_unforked() {
    require_tmux!();

    let mut h = TuiTestHarness::new("opencode_store_fork_refusal");
    let database = seed_store(&h);
    h.set_env("OPENCODE_DB", &database);
    // The agent answers `--help` but never serves, so readiness cannot complete.
    let _log = install_fake_opencode(&mut h, false);
    let project = h.project_path();

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "opencode",
        "-t",
        PARENT,
    ]);
    h.run_cli_ok(&["session", "set-session-id", PARENT, PARENT_ID]);
    let _cleanup = StopSessionsOnDrop { h: &h };

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "opencode",
        "-t",
        CHILD,
        "--fork-from",
        PARENT,
        "--launch",
    ]);
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(
        !add.status.success() || stderr.contains("refused"),
        "a fork with no reachable store must be refused, not silently unforked:\n{stderr}"
    );
}

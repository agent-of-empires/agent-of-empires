//! CLI coverage for store-backed OpenCode forks and managed host environments.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_python3, require_tmux, TuiTestHarness};

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

fn server_program() -> String {
    r#"import json, os, socket, sqlite3, sys
port = int(sys.argv[1])
body = json.loads(sys.argv[2])
mode = os.environ.get('AOE_FORK_FIXTURE_ROW', 'persisted')
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(('127.0.0.1', port))
srv.listen(16)
while True:
    conn, _ = srv.accept()
    try:
        request = conn.recv(65535)
        if request.startswith(b'POST '):
            parent = request.split()[1].decode().split('/')[-2]
            if mode == 'parent':
                body['data']['id'] = parent
            elif mode != 'missing':
                with sqlite3.connect(os.environ['OPENCODE_DB']) as database:
                    directory = database.execute('SELECT directory FROM session_v2 WHERE id=?', (parent,)).fetchone()[0]
                    table = 'session' if mode == 'inactive' else 'session_v2'
                    if mode == 'inactive':
                        database.execute('CREATE TABLE session AS SELECT * FROM session_v2 WHERE 0')
                    if mode == 'wrong-cwd': directory = os.path.dirname(directory)
                    if mode == 'relative': directory = '.'
                    workspace = 'remote' if mode == 'workspace' else None
                    database.execute('INSERT INTO ' + table + ' (id,project_id,directory,workspace_id,slug,version) SELECT ?,project_id,?,?,slug,version FROM session_v2 WHERE id=?', (body['data']['id'], directory, workspace, parent))
        encoded = json.dumps(body).encode()
        conn.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n' + ('Content-Length: %d\r\n\r\n' % len(encoded)).encode() + encoded)
    finally:
        conn.close()
"#.to_string()
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
    // The last non-probe invocation records the pane's actual launch.
    s.push_str(
        "if [ \"$1\" = \"--version\" ]; then printf '%s\\n' 'opencode v2.0.24'; exit 0; fi\n",
    );
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

/// Waits for the launch line, which is what the stub last wrote once the
/// ephemeral server has been reaped and stopped writing.
fn wait_for_launch(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let contents = fs::read_to_string(path).unwrap_or_default();
        // The stub records `<path> <argv...>`, so the generation probe line ends
        // in --help and the ephemeral server's names serve. Neither is the launch.
        let last = contents.trim();
        if !last.is_empty() && !last.contains("serve") && !last.ends_with("--help") {
            return last.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the launch"
        );
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

/// Pin the real SQLite store and seed only the parent. The server mints its child.
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
    require_python3!();

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
    // The stub is overwritten on every call, so what survives once the server
    // has been reaped is the launch itself.
    let launch = wait_for_launch(&launch_log);
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
    // The committed row must retain both the reusable child and its live store route.
    let child = h
        .read_sessions()
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["title"] == CHILD))
        .cloned()
        .expect("the child row");
    assert_eq!(
        child["agent_session_id"], CHILD_ID,
        "the row must carry the child the store minted: {child}"
    );
    assert!(
        child.get("resume_intent").is_none(),
        "the fork intent must be resolved once the child is adopted: {child}"
    );
    assert_eq!(
        child["active_execution"]["binding"]["agent"], "opencode",
        "{child}"
    );
    assert!(
        child["active_execution"]["binding"]["stores"]
            .as_array()
            .is_some_and(|stores| stores
                .iter()
                .any(|store| store.as_str() == Some(database.as_str()))),
        "the fork lost its live database route: {child}"
    );
}

#[test]
#[parallel]
fn opencode_store_fork_rejects_an_unattested_child_without_adoption() {
    require_tmux!();
    require_python3!();
    for (case, reason) in [
        ("missing", "active session table"),
        ("inactive", "active session table"),
        ("wrong-cwd", "working directory"),
        ("relative", "working directory"),
        ("workspace", "workspace"),
        ("parent", "instead of a child"),
    ] {
        let mut h = TuiTestHarness::new("opencode_fork_invalid_child");
        let database = seed_store(&h);
        h.set_env("OPENCODE_DB", &database);
        h.set_env("AOE_FORK_FIXTURE_ROW", case);
        let log = install_fake_opencode(&mut h, true);
        h.run_cli_ok(&[
            "add",
            h.project_path().to_str().unwrap(),
            "--cmd",
            "opencode",
            "-t",
            PARENT,
        ]);
        h.run_cli_ok(&["session", "set-session-id", PARENT, PARENT_ID]);
        h.run_cli_ok(&[
            "add",
            h.project_path().to_str().unwrap(),
            "--cmd",
            "opencode",
            "-t",
            CHILD,
            "--fork-from",
            PARENT,
        ]);
        let _cleanup = StopSessionsOnDrop { h: &h };
        let before = h.read_sessions();
        let start = h.run_cli(&["session", "start", CHILD]);
        let stderr = String::from_utf8_lossy(&start.stderr);
        assert!(!start.status.success(), "{case}: {stderr}");
        assert!(stderr.contains(reason), "{case}: {stderr}");
        let after = h.read_sessions();
        for title in [PARENT, CHILD] {
            let find = |rows: &serde_json::Value| {
                rows.as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["title"] == title)
                    .unwrap()
                    .clone()
            };
            let old = find(&before);
            let new = find(&after);
            for key in [
                "agent_session_id",
                "agent_session_binding",
                "resume_intent",
                "resume_binding",
            ] {
                assert_eq!(old.get(key), new.get(key), "{case}: {title} changed {key}");
            }
            assert!(new.get("active_execution").is_none(), "{case}: {new}");
        }
        let calls = fs::read_to_string(log).unwrap();
        assert!(
            calls.lines().any(|line| line.starts_with("serve ")),
            "{case}: {calls}"
        );
        assert!(
            calls
                .lines()
                .all(|line| line == "--help" || line == "--version" || line.starts_with("serve ")),
            "{case}: a refused child reached the pane: {calls}"
        );
        assert!(
            !h.tmux()
                .arg("has-session")
                .output()
                .unwrap()
                .status
                .success(),
            "{case}: a refused child opened a pane"
        );
    }
}

/// With no store reachable the fork must be refused rather than started
/// unforked, and the refusal must say so.
#[test]
#[parallel]
fn opencode_store_fork_refuses_rather_than_starting_unforked() {
    require_tmux!();
    require_python3!();

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
    // A refusal is not any failure: an unrelated early exit would satisfy a
    // bare status check. The message is what ties it to the fork.
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(
        !add.status.success() && stderr.contains("refused"),
        "a fork with no reachable store must be refused, not silently unforked: \
         status={}\n{stderr}",
        add.status
    );
}

#[test]
#[parallel]
fn opencode_host_launch_preserves_login_or_managed_environment() {
    require_tmux!();
    require_python3!();
    use std::os::unix::ffi::OsStrExt;
    for (mode, managed) in [
        ("current", false),
        ("legacy", false),
        ("external", false),
        ("current", true),
        ("legacy", true),
        ("wrapper", true),
    ] {
        let mut h = TuiTestHarness::new("opencode_host_environment");
        if managed {
            let database = h.home_path().join("store.db");
            rusqlite::Connection::open(&database).unwrap();
            h.set_env("OPENCODE_DB", database.as_os_str());
        } else {
            h.set_env("OPENCODE_DB", "");
            h.set_env("OPENCODE_DISABLE_CHANNEL_DB", "0");
        }
        h.append_config(
            "[session]\nagent_status_hooks=false\nsmart_rename=false\nname_agent_session=false",
        );
        let bin = h.install_path_command("opencode");
        let wrong = h.home_path().join("wrong");
        fs::create_dir(&wrong).unwrap();
        let output = h.home_path().join("frontend.json");
        let engine = if mode == "wrapper" {
            "engine"
        } else {
            "opencode"
        };
        let added = format!("LOGIN_ADDED_{}", uuid::Uuid::new_v4().simple());
        h.set_env("SHELL", "/bin/bash");
        h.set_env("LOGIN_GENERATION", "captured");
        h.set_env("LOGIN_ENGINE", bin.join("engine").to_str().unwrap());
        h.set_env("PRIVATE_EXEC_TOKEN", "dummy-frozen-secret");
        h.set_env("AOE.test-key", "line1\nline2 '\"$");
        h.set_env("BASH_FUNC_aoe_probe%%", "() { :; }");
        h.set_env("TMUX_PANE", "%parent");
        h.set_env("OPENCODE_PERMISSION", "captured-user-policy");
        h.set_env("AOE_RAW_VALUE", std::ffi::OsStr::from_bytes(b"\xff\xfe"));
        h.set_env(
            std::ffi::OsStr::from_bytes(b"AOE_RAW_\xff"),
            std::ffi::OsStr::from_bytes(b"\xfe\xfd"),
        );
        for directory in [&bin, &wrong] {
            let legacy = (mode == "legacy") != (directory == &wrong);
            let help = if legacy { "--fork" } else { "--auto" };
            let script = format!(
                r#"#!{python}
import json, os, sys
if sys.argv[1:] == ['--help']:
    print('{help}'); sys.exit(0)
def opened(fd):
    try: os.fstat(fd); return True
    except OSError: return False
record = dict(program=os.path.abspath(__file__), cwd=os.getcwd(), argv=sys.argv[1:], environment=dict((k,v) for k,v in os.environ.items() if not k.startswith('AOE_RAW')), raw=[os.environb.get(b'AOE_RAW_VALUE', b'').hex(), os.environb.get(b'AOE_RAW_\xff', b'').hex()], tty=os.isatty(0), descriptors=[opened(3), opened(4)])
with open({output:?}, 'w') as f: json.dump(record, f)
for line in sys.stdin: pass
"#,
                python = interpreter(),
                output = output.to_str().unwrap()
            );
            crate::harness::write_executable(&directory.join(engine), &script);
            if mode == "wrapper" {
                crate::harness::write_executable(&directory.join("opencode"), &format!(
                    "#!{}\nimport os, sys\nos.execv(os.environ['LOGIN_ENGINE'], [os.environ['LOGIN_ENGINE'], *sys.argv[1:]])\n", interpreter()
                ));
            }
        }
        fs::write(h.home_path().join(".bash_profile"), format!(
            "export PATH={}:{}\nexport LOGIN_GENERATION=login\nexport {added}=added\nexport OPENAI_API_KEY=dummy-login-key\nexport SSH_AUTH_SOCK=/login/agent.sock\nexport HTTPS_PROXY=http://login.invalid\nexport PRIVATE_EXEC_TOKEN=login-secret\nexport LOGIN_ENGINE={}\ncd /\n",
            wrong.display(), std::env::var("PATH").unwrap_or_default(), wrong.join("engine").display(),
        )).unwrap();
        let command = if mode == "external" {
            "opencode --session external"
        } else {
            "opencode"
        };
        h.run_cli_ok(&[
            "add",
            h.project_path().to_str().unwrap(),
            "--cmd",
            command,
            "-t",
            "FrozenContext",
            "--yolo",
            "--launch",
        ]);
        let record: serde_json::Value =
            crate::harness::wait_until(Duration::from_secs(20), Duration::from_millis(20), || {
                fs::read(&output)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
            });
        assert_eq!(record["program"], bin.join(engine).to_str().unwrap());
        assert_eq!(
            record["cwd"],
            fs::canonicalize(h.project_path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        let env = &record["environment"];
        if managed {
            assert_eq!(env["LOGIN_GENERATION"], "captured");
            assert_eq!(env["PRIVATE_EXEC_TOKEN"], "dummy-frozen-secret");
            assert_eq!(env["AOE.test-key"], "line1\nline2 '\"$");
            assert_eq!(env["BASH_FUNC_aoe_probe%%"], "() { :; }");
            assert!(env.get(&added).is_none());
            assert_eq!(record["raw"], serde_json::json!(["fffe", "fefd"]));
        } else {
            assert_eq!(env["LOGIN_GENERATION"], "login");
            assert_eq!(env["PRIVATE_EXEC_TOKEN"], "login-secret");
            assert_eq!(env["OPENAI_API_KEY"], "dummy-login-key");
            assert_eq!(env["SSH_AUTH_SOCK"], "/login/agent.sock");
            assert_eq!(env["HTTPS_PROXY"], "http://login.invalid");
            assert_eq!(env[&added], "added");
            assert!(env["PATH"]
                .as_str()
                .unwrap()
                .starts_with(wrong.to_str().unwrap()));
            assert_eq!(record["raw"][0], "fffe");
        }
        assert_ne!(env["TMUX_PANE"], "%parent");
        assert!(env["TMUX_PANE"].as_str().unwrap().starts_with('%'));
        assert_eq!(record["tty"], true);
        if managed {
            assert_eq!(record["descriptors"], serde_json::json!([false, false]));
        }
        let argv = record["argv"].as_array().unwrap();
        assert_eq!(argv.iter().any(|arg| arg == "--auto"), mode != "legacy");
        if mode == "legacy" {
            assert_eq!(env["OPENCODE_PERMISSION"], r#"{"*":"allow"}"#);
        } else {
            assert_eq!(env["OPENCODE_PERMISSION"], "captured-user-policy");
        }
        if mode == "external" {
            assert_eq!(
                record["argv"],
                serde_json::json!(["--session", "external", "--auto"])
            );
        }
    }
}

#[test]
#[parallel]
fn opencode_legacy_fork_adoption_uses_the_prepared_strategy() {
    require_tmux!();
    require_python3!();
    let mut h = TuiTestHarness::new("opencode_prepared_fork");
    h.append_config(
        "[session]\nagent_status_hooks=false\nsmart_rename=false\nname_agent_session=false",
    );
    let database = seed_store(&h);
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("ALTER TABLE session_v2 RENAME TO session")
        .unwrap();
    h.set_env("OPENCODE_DB", &database);
    let bin = h.install_path_command("opencode");
    let first = h.home_path().join("first-help");
    let launched = h.home_path().join("launched.json");
    let script = format!(
        r#"#!{python}
import json, pathlib, sys, time
first = pathlib.Path({first:?})
launched = pathlib.Path({launched:?})
if sys.argv[1:] == ['--version']:
    print('1.18.29'); sys.exit(0)
if sys.argv[1:] == ['--help']:
    if not first.exists():
        first.touch(); print('--fork'); sys.exit(0)
    end = time.monotonic() + 10
    while not launched.exists():
        if time.monotonic() >= end: sys.exit(1)
        time.sleep(.01)
    print('--auto'); sys.exit(0)
with launched.open('w') as f: json.dump(sys.argv[1:], f)
for line in sys.stdin: pass
"#,
        python = interpreter(),
        first = first.to_str().unwrap(),
        launched = launched.to_str().unwrap()
    );
    crate::harness::write_executable(&bin.join("opencode"), &script);
    h.run_cli_ok(&[
        "add",
        h.project_path().to_str().unwrap(),
        "--tool",
        "opencode",
        "-t",
        PARENT,
    ]);
    h.run_cli_ok(&["session", "set-session-id", PARENT, PARENT_ID]);
    h.run_cli_ok(&[
        "add",
        h.project_path().to_str().unwrap(),
        "--tool",
        "opencode",
        "-t",
        CHILD,
        "--fork-from",
        PARENT,
        "--launch",
    ]);
    let argv: serde_json::Value =
        crate::harness::wait_until(Duration::from_secs(20), Duration::from_millis(20), || {
            fs::read(&launched)
                .map_err(|e| e.to_string())
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        });
    assert!(argv.as_array().unwrap().iter().any(|arg| arg == "--fork"));
    let sessions = h.read_sessions();
    let child = crate::harness::session_by_title(&sessions, CHILD);
    assert!(
        child["agent_session_id"].is_null(),
        "a root --fork cannot resume AoE's unused preallocated seed: {child}"
    );
}

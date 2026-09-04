//! Full-stack e2e: two Hermes sessions on DIFFERENT projects each resume
//! their OWN active Hermes conversation and never adopt the peer's.
//!
//! Guards #3373: `capture_hermes_session_id` used to return the most recent
//! active CLI conversation regardless of project, so with two Hermes sessions
//! on different projects the second session to poll could adopt the first's
//! conversation and resume bound the wrong conversation. The fix scopes the
//! capture to the session's project via the `cwd`/`git_repo_root` columns
//! Hermes records on each CLI session row.
//!
//! Teeth: `aoe session start` is a blocking CLI launch that captures and
//! persists the session id via `capture_launched_session_id_blocking`
//! (defined in src/session/sync.rs, called from src/cli/session.rs; an 8s
//! bounded drain of the session's poller). With
//! the project scoping reverted, session A's start captures B (the global
//! most-recent conversation) and publishes `AOE_CAPTURED_SESSION_ID`; session
//! B's start then excludes B and captures A, a deterministic swap (or both
//! capture B under an alternate publish-timing interleaving). Every
//! interleaving fails the positive per-session assertions below; a revert can
//! never yield `[A] == own-id` for both sessions. Asserting per-session
//! equality (never A != B) is what gives the test teeth, mirroring
//! `claude_shared_project_correlation_e2e`.
//!
//! The e2e covers the host capture path only; the sandboxed container path is
//! covered by unit tests over `HERMES_CONTAINER_CAPTURE_SCRIPT` and
//! `select_hermes_session_in_container` in src/session/capture/mod.rs.
//!
//! Daemon-free, so no feature gate. Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- hermes_shared_project_correlation --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

// Seeded Hermes conversation ids, one per project. Each session must end up
// persisting its own; the global most-recent conversation under a reverted
// fix is B (higher started_at).
const CONV_A: &str = "20260101_000000_aaaa";
const CONV_B: &str = "20260101_000000_bbbb";

// Deadline and cadence for the correlation wait, and the deadline for a shim
// to publish its marker.
const CORRELATE_DEADLINE: Duration = Duration::from_secs(30);
const CORRELATE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHIM_DEADLINE: Duration = Duration::from_secs(10);
const SHIM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Parse the `  ID:      <id>` line that `aoe add` prints on success.
fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// Seed the fake Hermes state.db with two active CLI conversations, one per
/// project. Uses the full schema (cwd + git_repo_root) and a post-seed
/// self-check: if the columns are missing the e2e would silently become a
/// false negative, so the seed fails loudly instead.
fn seed_hermes_state_db(h: &TuiTestHarness, proj_a: &std::path::Path, proj_b: &std::path::Path) {
    let canon_a = std::fs::canonicalize(proj_a).expect("canonicalize proj-a");
    let canon_b = std::fs::canonicalize(proj_b).expect("canonicalize proj-b");
    let db_path = h.home_path().join(".hermes").join("state.db");
    std::fs::create_dir_all(db_path.parent().expect("hermes home parent"))
        .expect("mkdir hermes home");

    let conn = rusqlite::Connection::open(&db_path).expect("open seeded hermes state.db");
    conn.execute_batch(&format!(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);
         INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('{CONV_A}','cli',1000.0,NULL,'{}',NULL);
         INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('{CONV_B}','cli',2000.0,NULL,'{}',NULL);",
        canon_a.to_string_lossy(),
        canon_b.to_string_lossy(),
    ))
    .expect("seed hermes state.db");
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(sessions)")
        .expect("pragma table_info")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("pragma rows")
        .map(|c| c.expect("pragma column"))
        .collect();
    drop(conn);
    assert!(
        cols.iter().any(|c| c == "cwd") && cols.iter().any(|c| c == "git_repo_root"),
        "seeded hermes state.db must carry cwd/git_repo_root columns, got: {cols:?}"
    );
}

/// Install a `hermes` shim on PATH that stays alive (`exec sleep 600`) so the
/// pane stays live for the poller host and `build_exclusion_set`'s peer scan.
/// The uuid-map marker proves the launch env carried `AOE_INSTANCE_ID`.
fn install_hermes_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("hermes");
    let script = r#"#!/bin/sh
if [ -z "$AOE_INSTANCE_ID" ]; then
  echo "missing AOE_INSTANCE_ID" > "$HOME/shim-missing-instance.marker"
  exit 3
fi
mkdir -p "$HOME/uuid-map"
printf '%s' "$AOE_INSTANCE_ID" > "$HOME/uuid-map/$AOE_INSTANCE_ID"
exec sleep 600
"#;
    let path = bin.join("hermes");
    std::fs::write(&path, script).expect("write hermes shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

/// Read sessions.json, tolerating a missing or mid-write file (returns Null).
fn read_sessions(h: &TuiTestHarness) -> Value {
    let content = std::fs::read_to_string(sessions_path(h)).unwrap_or_default();
    serde_json::from_str(&content).unwrap_or(Value::Null)
}

fn agent_session_id_of(sessions: &Value, instance_id: &str) -> Option<String> {
    sessions
        .as_array()?
        .iter()
        .find(|r| r["id"].as_str() == Some(instance_id))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

/// Wait until each session's persisted `agent_session_id` equals its own
/// seeded conversation id, and stays there across two consecutive reads. The
/// stability check rejects a mid-convergence transient and a flip-flopping
/// regression alike. On timeout it panics with a full diagnostic dump.
fn await_correlated(h: &TuiTestHarness, id_a: &str, id_b: &str) {
    let deadline = Instant::now() + CORRELATE_DEADLINE;
    let mut consecutive_ok = 0;
    loop {
        let sessions = read_sessions(h);
        let a = agent_session_id_of(&sessions, id_a);
        let b = agent_session_id_of(&sessions, id_b);
        if a.as_deref() == Some(CONV_A) && b.as_deref() == Some(CONV_B) {
            consecutive_ok += 1;
            if consecutive_ok >= 2 {
                return;
            }
        } else {
            consecutive_ok = 0;
        }
        if Instant::now() >= deadline {
            let shim_no_inst = h.home_path().join("shim-missing-instance.marker").exists();
            let debug_log = std::fs::read_to_string(app_dir_in(h.home_path()).join("debug.log"))
                .unwrap_or_default();
            let dbg_lines: Vec<&str> = debug_log
                .lines()
                .filter(|l| {
                    l.contains("session.sync")
                        || l.contains("session.capture")
                        || l.contains("session.store")
                })
                .collect();
            let dbg_tail = dbg_lines
                .iter()
                .rev()
                .take(40)
                .rev()
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "session ids never correlated within {CORRELATE_DEADLINE:?}.\n\
                 expected: [{id_a}]={CONV_A}, [{id_b}]={CONV_B}\n\
                 observed: [{id_a}]={a:?}, [{id_b}]={b:?}\n\
                 AOE_INSTANCE_ID-missing marker: {shim_no_inst}\n\
                 debug.log (sync/capture/store):\n{dbg_tail}\n\
                 sessions.json:\n{}",
                serde_json::to_string_pretty(&sessions).unwrap_or_default(),
            );
        }
        std::thread::sleep(CORRELATE_POLL_INTERVAL);
    }
}

/// Block until the shim for `instance_id` recorded its uuid-map marker.
fn wait_for_shim(h: &TuiTestHarness, instance_id: &str) {
    let deadline = Instant::now() + SHIM_DEADLINE;
    while Instant::now() < deadline {
        if h.home_path().join("uuid-map").join(instance_id).exists() {
            return;
        }
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }
    let missing_inst = h.home_path().join("shim-missing-instance.marker").exists();
    panic!(
        "shim for {instance_id} never wrote its uuid-map entry within {SHIM_DEADLINE:?} \
         (AOE_INSTANCE_ID-missing marker: {missing_inst})"
    );
}

/// Create a session with `aoe add` (no launch); returns the instance id.
fn add_session(h: &TuiTestHarness, project: &std::path::Path, title: &str) -> String {
    let add = h.run_cli(&[
        "add",
        project.to_str().expect("utf8 project"),
        "-t",
        title,
        "-c",
        "hermes",
    ]);
    assert!(
        add.status.success(),
        "aoe add {title} failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    parse_session_id(&String::from_utf8_lossy(&add.stdout))
}

/// Launch a session's tmux pane with `aoe session start` (a deterministic,
/// blocking CLI launch that captures and persists the session id before
/// returning, which is what gives the assertions their teeth).
fn launch_session(h: &TuiTestHarness, instance_id: &str) {
    let start = h.run_cli(&["session", "start", instance_id]);
    assert!(
        start.status.success(),
        "aoe session start {instance_id} failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
}

#[test]
#[parallel]
fn hermes_shared_project_correlation() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("hermes_shared_project_correlation");
    // The poller host (TUI) and every CLI subprocess inherit HERMES_HOME via
    // the harness's extra_env plumbing.
    let hermes_home = h.home_path().join(".hermes");
    h.set_env("HERMES_HOME", &hermes_home.display().to_string());
    install_hermes_shim(&mut h);

    // Two DIFFERENT project dirs. `aoe add` bails on a missing dir, so create
    // them before canonicalizing and seeding.
    let proj_a = h.home_path().join("proj-a");
    let proj_b = h.home_path().join("proj-b");
    std::fs::create_dir_all(&proj_a).expect("mkdir proj-a");
    std::fs::create_dir_all(&proj_b).expect("mkdir proj-b");
    seed_hermes_state_db(&h, &proj_a, &proj_b);

    // Add both sessions (no launch) so their ids and sessions.json rows exist.
    let id_a = add_session(&h, &proj_a, "hermes-A");
    let id_b = add_session(&h, &proj_b, "hermes-B");

    // Launch both; each CLI start captures and persists its own conversation
    // before returning.
    launch_session(&h, &id_a);
    launch_session(&h, &id_b);

    // Both shims ran with their AOE_INSTANCE_ID (launch env carried it).
    wait_for_shim(&h, &id_a);
    wait_for_shim(&h, &id_b);

    // Attach the poller host (the native TUI drains poller observations into
    // sessions.json; the CLI one-shot launches already persisted the ids).
    h.spawn_tui();
    h.wait_for_ready();

    await_correlated(&h, &id_a, &id_b);
}

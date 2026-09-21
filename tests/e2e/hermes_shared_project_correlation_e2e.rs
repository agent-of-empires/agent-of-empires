//! Full-stack e2e: host Hermes state is not an authoritative identity source.
//!
//! Two real-looking active conversations are seeded in the shared Hermes
//! database. Host launches must ignore both and leave agent_session_id empty;
//! managed sandbox stores are covered by unit tests in src/session/capture/mod.rs.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{agent_session_id_of, require_tmux, write_executable, TuiTestHarness};

// Seeded Hermes conversation ids, one per project. The global most-recent
// conversation under a reverted fix is B (higher started_at).
const CONV_A: &str = "20260101_000000_aaaa";
const CONV_B: &str = "20260101_000000_bbbb";

// Deadline and cadence for a shim to publish its marker.
const SHIM_DEADLINE: Duration = Duration::from_secs(10);
const SHIM_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FAIL_CLOSED_OBSERVATION: Duration = Duration::from_secs(5);

/// Seed the fake Hermes state.db with one active CLI conversation per project.
/// The post-seed column check keeps a schema drift from turning this into a
/// false negative.
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

/// A `hermes` shim that stays alive so the pane stays live for the poller host,
/// and whose uuid-map marker proves the launch env carried `AOE_INSTANCE_ID`.
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
    write_executable(&bin.join("hermes"), script);
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

#[test]
#[parallel]
fn hermes_host_capture_fails_closed() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("hermes_host_capture_fails_closed");
    let hermes_home = h.home_path().join(".hermes");
    h.set_env("HERMES_HOME", &hermes_home.display().to_string());
    install_hermes_shim(&mut h);

    let proj_a = h.home_path().join("proj-a");
    let proj_b = h.home_path().join("proj-b");
    std::fs::create_dir_all(&proj_a).expect("mkdir proj-a");
    std::fs::create_dir_all(&proj_b).expect("mkdir proj-b");
    seed_hermes_state_db(&h, &proj_a, &proj_b);

    let id_a = h.add_session(&[proj_a.to_str().unwrap(), "-t", "hermes-A", "-c", "hermes"]);
    let id_b = h.add_session(&[proj_b.to_str().unwrap(), "-t", "hermes-B", "-c", "hermes"]);
    // `session start` is a blocking launch, so host capture has finished.
    h.run_cli_ok(&["session", "start", &id_a]);
    h.run_cli_ok(&["session", "start", &id_b]);
    wait_for_shim(&h, &id_a);
    wait_for_shim(&h, &id_b);

    h.spawn_tui();
    h.wait_for_ready();

    let deadline = Instant::now() + FAIL_CLOSED_OBSERVATION;
    loop {
        let sessions = h.try_read_sessions();
        assert_eq!(
            agent_session_id_of(&sessions, &id_a),
            None,
            "Hermes host capture assigned an unverified identity to {id_a}"
        );
        assert_eq!(
            agent_session_id_of(&sessions, &id_b),
            None,
            "Hermes host capture assigned an unverified identity to {id_b}"
        );
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }
}

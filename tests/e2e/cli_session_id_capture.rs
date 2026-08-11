//! E2E: a CLI-launched, capture-deferred agent must persist
//! `agent_session_id` with no `aoe serve` daemon and no TUI running.
//!
//! Uses a fake `codex` (a capture-deferred agent) that, on launch, writes a
//! rollout file the codex poller scans, then idles so its tmux pane stays
//! alive. Before the fix, `aoe session start` returned before draining the
//! poller, so the observed id was dropped and `sessions.json` kept
//! `agent_session_id: null`, silently breaking resume.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

const TITLE: &str = "CliSidCaptureE2E";
const FAKE_SID: &str = "019342ab-1234-7def-8901-abcdef012345";

fn new_harness(name: &str) -> TuiTestHarness {
    #[cfg(unix)]
    {
        TuiTestHarness::new_in_tmp(name)
    }
    #[cfg(not(unix))]
    {
        TuiTestHarness::new(name)
    }
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn agent_session_id(h: &TuiTestHarness, title: &str) -> Option<String> {
    let content = fs::read_to_string(sessions_path(h)).ok()?;
    let sessions: Value = serde_json::from_str(&content).ok()?;
    sessions
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn omp_capture_generation(h: &TuiTestHarness, title: &str) -> Option<String> {
    let content = fs::read_to_string(sessions_path(h)).ok()?;
    let sessions: Value = serde_json::from_str(&content).ok()?;
    sessions
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))?
        .get("omp_capture_generation")?
        .as_str()
        .map(str::to_owned)
}
fn clear_omp_capture_generation(h: &TuiTestHarness, title: &str) {
    let path = sessions_path(h);
    let mut sessions: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read sessions")).unwrap();
    let session = sessions
        .as_array_mut()
        .and_then(|rows| {
            rows.iter_mut()
                .find(|row| row["title"].as_str() == Some(title))
        })
        .expect("find OMP session row");
    session
        .as_object_mut()
        .expect("session row is an object")
        .remove("omp_capture_generation");
    fs::write(
        path,
        serde_json::to_vec_pretty(&sessions).expect("serialize sessions"),
    )
    .expect("write legacy sessions row");
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn install_fake_codex(h: &mut TuiTestHarness, codex_home: &Path, project: &Path) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let rollout = sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{FAKE_SID}.jsonl"));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nprintf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {file}\nexec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        cwd = sh_quote(project),
        file = sh_quote(&rollout),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
    title: &'a str,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", self.title]);
    }
}

#[test]
#[parallel]
fn cli_session_start_persists_agent_session_id_without_daemon() {
    require_tmux!();
    let mut h = new_harness("cli_sid_capture");
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_fake_codex(&mut h, &codex_home, &project);

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-c", "codex", "-t", TITLE]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    assert_eq!(
        agent_session_id(&h, TITLE),
        None,
        "agent_session_id must be unset before launch"
    );

    let _stop = StopSessionOnDrop {
        h: &h,
        title: TITLE,
    };
    let start = h.run_cli(&["session", "start", TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    assert_eq!(
        agent_session_id(&h, TITLE).as_deref(),
        Some(FAKE_SID),
        "CLI launch must drain the poller and persist agent_session_id \
         without a daemon or TUI"
    );
}

const RESTART_TITLE: &str = "CliSidRestartE2E";
const RESTART_SID_FIRST: &str = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
const RESTART_SID_SECOND: &str = "019342ab-1234-7def-8901-bbbbbbbbbbbb";

/// Fake codex that mints `RESTART_SID_FIRST` on its first launch and
/// `RESTART_SID_SECOND` on every launch after (tracked by a marker file). The
/// codex poller picks the newest rollout by mtime. On restart the second
/// rollout is written after a 1s delay, so the freshly-started poller's
/// immediate first poll sees only the lingering first rollout: without the
/// forced-fresh exclusion the restart drain would capture and persist the
/// stale `RESTART_SID_FIRST` and stop waiting; the exclusion makes it reject
/// that and wait for the second rollout, which lands well inside the bound.
fn install_toggling_fake_codex(h: &mut TuiTestHarness, codex_home: &Path, project: &Path) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let marker = codex_home.join(".launched");
    let rollout_first = sessions_dir.join(format!(
        "rollout-2025-01-01T00-00-00-{RESTART_SID_FIRST}.jsonl"
    ));
    let rollout_second = sessions_dir.join(format!(
        "rollout-2025-01-02T00-00-00-{RESTART_SID_SECOND}.jsonl"
    ));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nif [ -f {marker} ]; then\n  sleep 1\n  \
         printf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {second}\nelse\n  \
         : > {marker}\n  printf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {first}\nfi\n\
         exec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        marker = sh_quote(&marker),
        cwd = sh_quote(project),
        first = sh_quote(&rollout_first),
        second = sh_quote(&rollout_second),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

/// Force `session restart` down the fresh-launch path (no `--resume <sid>`), so
/// the restarted agent mints a new capture-deferred sid the restart path must
/// drain. Without `auto_resume_on_restart = false` the restart resumes the
/// existing sid and never observes a new one.
///
/// Also blanks `restart_wake_message`, which removes the pre-capture
/// `wait_for_pane_ready` wake wait. Without that, the capture helper drains the
/// fresh poller immediately after relaunch, while only the lingering first
/// rollout exists (the second is written 1s later), so the forced-fresh
/// exclusion is load-bearing: without it the drain persists the stale
/// `RESTART_SID_FIRST` and returns. With the wake wait in place the poller
/// would observe both rollouts before the drain and pick the newest anyway,
/// masking the exclusion.
fn configure_fresh_restart_capture(h: &TuiTestHarness) {
    let config = app_dir_in(h.home_path()).join("config.toml");
    let mut doc = fs::read_to_string(&config)
        .unwrap_or_default()
        .parse::<toml_edit::DocumentMut>()
        .expect("parse config.toml");
    doc["session"]["auto_resume_on_restart"] = toml_edit::value(false);
    doc["session"]["restart_wake_message"] = toml_edit::value("");
    fs::write(&config, doc.to_string()).expect("write config.toml");
}

#[test]
#[parallel]
fn cli_session_restart_persists_new_agent_session_id_without_daemon() {
    require_tmux!();
    let mut h = new_harness("cli_sid_restart");
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_toggling_fake_codex(&mut h, &codex_home, &project);
    configure_fresh_restart_capture(&h);

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-c",
        "codex",
        "-t",
        RESTART_TITLE,
    ]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let _stop = StopSessionOnDrop {
        h: &h,
        title: RESTART_TITLE,
    };
    let start = h.run_cli(&["session", "start", RESTART_TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert_eq!(
        agent_session_id(&h, RESTART_TITLE).as_deref(),
        Some(RESTART_SID_FIRST),
        "start must capture the first sid"
    );

    let restart = h.run_cli(&["session", "restart", RESTART_TITLE]);
    assert!(
        restart.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert_eq!(
        agent_session_id(&h, RESTART_TITLE).as_deref(),
        Some(RESTART_SID_SECOND),
        "`session restart` must drain the fresh poller and persist the new \
         agent_session_id without a daemon or TUI"
    );
}

const LAZY_TITLE: &str = "CliSidLazyE2E";
const LAZY_SID: &str = "019342ab-1234-7def-8901-cccccccccccc";

/// Fake codex whose session store is populated LAZILY: on launch it writes
/// nothing and idles, and only writes its rollout once a marker file (standing
/// in for opencode's first-user-turn SQLite write) appears. This reproduces
/// the residual gap #3177 missed: the launch-time bounded wait finds no store
/// entry (the agent has not created it yet), so with no daemon or TUI the id
/// would stay `None` forever. The read-command self-heal must backfill it once
/// the store entry exists.
fn install_lazy_fake_codex(
    h: &mut TuiTestHarness,
    codex_home: &Path,
    project: &Path,
    marker: &Path,
) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let rollout = sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{LAZY_SID}.jsonl"));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nwhile [ ! -f {marker} ]; do sleep 0.1; done\n\
         printf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {file}\nexec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        marker = sh_quote(marker),
        cwd = sh_quote(project),
        file = sh_quote(&rollout),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write lazy fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

#[test]
#[parallel]
fn read_command_backfills_agent_session_id_for_a_lazy_store_write() {
    require_tmux!();
    let mut h = new_harness("cli_sid_lazy");
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    let marker = codex_home.join(".first-turn");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_lazy_fake_codex(&mut h, &codex_home, &project, &marker);

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-c",
        "codex",
        "-t",
        LAZY_TITLE,
    ]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let _stop = StopSessionOnDrop {
        h: &h,
        title: LAZY_TITLE,
    };
    let start = h.run_cli(&["session", "start", LAZY_TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert_eq!(
        agent_session_id(&h, LAZY_TITLE),
        None,
        "launch-time capture must miss: the agent has not written its store entry yet"
    );

    fs::write(&marker, b"").expect("touch first-turn marker");
    let rollout = codex_home
        .join("sessions")
        .join(format!("rollout-2025-01-01T00-00-00-{LAZY_SID}.jsonl"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !rollout.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        rollout.exists(),
        "fake codex did not write its rollout within 5s; the backfill assertion \
         below would otherwise fail for the wrong reason"
    );

    let status = h.run_cli(&["status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        agent_session_id(&h, LAZY_TITLE).as_deref(),
        Some(LAZY_SID),
        "`aoe status` must backfill agent_session_id once the store entry exists, \
         with no daemon or TUI"
    );
}

fn install_fake_codex_with_sid(
    h: &mut TuiTestHarness,
    codex_home: &Path,
    project: &Path,
    sid: &str,
) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let rollout = sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{sid}.jsonl"));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nprintf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {file}\nexec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        cwd = sh_quote(project),
        file = sh_quote(&rollout),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

/// Shared body for the read-command self-heal guard tests: add a
/// capture-deferred fake-codex session, start it (so a rollout lingers and the
/// tmux pane is live), then patch the on-disk row into the exact state the
/// guard under test must skip, run `aoe status`, and assert the sid stays
/// `None`. Each guard has a distinct on-disk precondition (`patch`), so they
/// stay separate `#[test]` fns that can fail independently, but the identical
/// scaffolding lives here once. `reason` names the guard for the assertion.
fn assert_self_heal_skips(
    harness_name: &str,
    title: &str,
    sid: &str,
    reason: &str,
    patch: impl FnOnce(&mut serde_json::Map<String, Value>),
) {
    let mut h = new_harness(harness_name);
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_fake_codex_with_sid(&mut h, &codex_home, &project, sid);

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-c", "codex", "-t", title]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let _stop = StopSessionOnDrop { h: &h, title };
    let start = h.run_cli(&["session", "start", title]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert_eq!(
        agent_session_id(&h, title).as_deref(),
        Some(sid),
        "setup: start must capture the sid, so the later None is the guard's doing, \
         not a capture that never happened"
    );

    patch_session_row(&h, title, patch);
    assert_eq!(
        agent_session_id(&h, title),
        None,
        "precondition: the on-disk sid is nulled while its rollout lingers"
    );

    let status = h.run_cli(&["status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(agent_session_id(&h, title), None, "{reason}");
}

fn patch_session_row(
    h: &TuiTestHarness,
    title: &str,
    patch: impl FnOnce(&mut serde_json::Map<String, Value>),
) {
    let path = sessions_path(h);
    let content = fs::read_to_string(&path).expect("read sessions.json");
    let mut sessions: Value = serde_json::from_str(&content).expect("parse sessions.json");
    let arr = sessions.as_array_mut().expect("sessions is an array");
    let row = arr
        .iter_mut()
        .find(|s| s["title"].as_str() == Some(title))
        .expect("session row present");
    patch(row.as_object_mut().expect("row is an object"));
    fs::write(
        &path,
        serde_json::to_string_pretty(&sessions).expect("serialize"),
    )
    .expect("write sessions.json");
}

/// Regression lock for the #3177 abandoned-sid contract, in the read-command
/// self-heal path. A session the user cleared (`ResumeIntent::Cleared`, sid
/// nulled) whose rollout still lingers newest-on-disk must NOT have that sid
/// re-adopted by a later `aoe status`: `self_heal_session_id` heals only
/// sessions with a plain (`Default`) resume intent. Because
/// `retroactive_capture_excludes` is runtime-only (empty on a fresh disk
/// load), the `Default` gate is the sole thing standing between the read
/// command and silently reverting the user's clear, so it is exercised
/// directly: the on-disk row is put into the exact (sid=None, Cleared) state a
/// forced-fresh launch leaves behind while the abandoned rollout is still the
/// only one on disk.
#[test]
#[parallel]
fn read_command_self_heal_does_not_readopt_a_cleared_sid() {
    require_tmux!();
    assert_self_heal_skips(
        "cli_sid_cleared",
        "CliSidClearedE2E",
        "019342ab-1234-7def-8901-dddddddddddd",
        "`aoe status` self-heal must NOT re-adopt a user-cleared sid whose rollout \
         still lingers newest-on-disk",
        |obj| {
            obj.insert("agent_session_id".to_string(), Value::Null);
            obj.insert(
                "resume_intent".to_string(),
                serde_json::json!({ "kind": "Cleared" }),
            );
        },
    );
}

/// A row in a mid-teardown / mid-creation status (`Deleting` / `Creating`)
/// must not be healed by a read command even with a live tmux pane and a
/// lingering rollout: touching a row another operation owns is exactly what
/// the status gate in `self_heal_session_id` prevents.
#[test]
#[parallel]
fn read_command_self_heal_skips_mid_operation_rows() {
    require_tmux!();
    assert_self_heal_skips(
        "cli_sid_midop",
        "CliSidMidOpE2E",
        "019342ab-1234-7def-8901-eeeeeeeeeeee",
        "`aoe status` self-heal must NOT touch a row in a mid-operation status",
        |obj| {
            obj.insert("agent_session_id".to_string(), Value::Null);
            // Status serializes lowercase (`#[serde(rename_all = "lowercase")]`).
            obj.insert("status".to_string(), Value::String("deleting".to_string()));
        },
    );
}

/// An archived (or trashed) row is a sink a read command must not mutate, and
/// `--no-kill` archiving or a not-yet-torn-down trashed row can still own a
/// live tmux pane, so neither the status gate nor the live-tmux gate catches
/// it. The active-bucket gate in `self_heal_session_id` must: `aoe status`
/// must not write `agent_session_id` back into an archived row even with a
/// lingering rollout and a live pane.
#[test]
#[parallel]
fn read_command_self_heal_skips_archived_rows() {
    require_tmux!();
    assert_self_heal_skips(
        "cli_sid_archived",
        "CliSidArchivedE2E",
        "019342ab-1234-7def-8901-ffffffffffff",
        "`aoe status` self-heal must NOT touch an archived (non-active bucket) row",
        |obj| {
            obj.insert("agent_session_id".to_string(), Value::Null);
            obj.insert(
                "archived_at".to_string(),
                Value::String("2026-01-01T00:00:00Z".to_string()),
            );
        },
    );
}

/// Installs a fake `opencode` that idles (so its tmux pane stays live) and
/// never writes a session store: the store row is written by the test itself
/// via rusqlite, mimicking opencode's lazy first-turn write. Exercises the
/// REAL opencode capture path (SQLite read + directory canonicalization) that
/// the jsonl-based fake-codex tests cannot reach.
fn install_idle_fake_opencode(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("opencode");
    let script_path = bin.join("opencode");
    fs::write(&script_path, "#!/bin/sh\nexec sleep 300\n").expect("write fake opencode");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("chmod opencode");
    }
}

/// Write a minimal `opencode.db` with one `session` row, matching the schema
/// `try_capture_opencode_session_id` reads (`SELECT id, directory, time_updated
/// FROM session`).
fn write_opencode_session_row(db_path: &Path, session_id: &str, directory: &str) {
    use rusqlite::{params, Connection};
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("create opencode data dir");
    let conn = Connection::open(db_path).expect("open opencode.db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
             id           TEXT NOT NULL,
             directory    TEXT NOT NULL,
             time_updated INTEGER NOT NULL
         );",
    )
    .expect("create session table");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    conn.execute(
        "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
        params![session_id, directory, now_ms],
    )
    .expect("insert session row");
}

/// End-to-end lock for the REAL opencode path through the actual `aoe` binary:
/// `aoe add --cmd opencode` + `aoe session start` leaves `agent_session_id`
/// null (opencode writes its SQLite row lazily), then once the row exists a
/// read-only `aoe status` backfills the id from `opencode.db` with no daemon or
/// TUI. The fake-codex e2e proves the status->self_heal->persist wiring; this
/// proves the opencode-specific SQLite read + directory match.
#[test]
#[parallel]
fn read_command_backfills_agent_session_id_for_opencode_sqlite_path() {
    require_tmux!();
    let mut h = new_harness("cli_sid_opencode_sqlite");
    let project = h.project_path();
    let canonical = fs::canonicalize(&project).unwrap_or_else(|_| project.clone());
    let canonical_str = canonical.to_str().expect("utf8 project path").to_string();

    // Pin opencode's DB to an explicit path via the same env var opencode reads,
    // so the test controls it independently of XDG_DATA_HOME.
    let db_path = h.home_path().join("opencode-data").join("opencode.db");
    h.set_env("OPENCODE_DB", db_path.to_str().expect("utf8 db path"));
    install_idle_fake_opencode(&mut h);

    const OC_TITLE: &str = "CliSidOpencodeE2E";
    const OC_SID: &str = "ses_0abcdefghijklmnopqrstuvwxy";

    let add = h.run_cli(&["add", &canonical_str, "-c", "opencode", "-t", OC_TITLE]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let _stop = StopSessionOnDrop {
        h: &h,
        title: OC_TITLE,
    };
    let start = h.run_cli(&["session", "start", OC_TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert_eq!(
        agent_session_id(&h, OC_TITLE),
        None,
        "launch-time capture must miss: opencode has not written its DB row yet"
    );

    write_opencode_session_row(&db_path, OC_SID, &canonical_str);

    let status = h.run_cli(&["status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        agent_session_id(&h, OC_TITLE).as_deref(),
        Some(OC_SID),
        "`aoe status` must backfill agent_session_id from a real opencode.db, \
         with no daemon or TUI"
    );
}

const OMP_TITLE_FIRST: &str = "CliSidOmpFirstE2E";
const OMP_TITLE_SECOND: &str = "CliSidOmpSecondE2E";
const OMP_SID_FIRST: &str = "019342ab-1234-7def-8901-cccccccccccc";
const OMP_SID_SECOND: &str = "019342ab-1234-7def-8901-dddddddddddd";
const OMP_SID_THIRD: &str = "019342ab-1234-7def-8901-ffffffffffff";
const OMP_STALE_SID: &str = "019342ab-1234-7def-8901-eeeeeeeeeeee";
const OMP_CAPTURE_META_KEY: &str = "AOE_OMP_CAPTURE_META";
const OMP_LAUNCH_ID_KEY: &str = "AOE_OMP_LAUNCH_ID";
const OMP_CAPTURE_READY_KEY: &str = "AOE_OMP_CAPTURE_READY";
const OMP_ROUTING_SECRET: &str = "/aoe-e2e-sensitive-routing-value";

fn write_project_omp_dotenv(project: &Path, store: &Path) {
    fs::write(
        project.join(".env"),
        format!(
            "OMP_CODING_AGENT_DIR={}\nPI_CONFIG_DIR={OMP_ROUTING_SECRET}\n",
            store.display()
        ),
    )
    .expect("write project OMP dotenv");
}

fn install_path_preserving_test_shell(h: &mut TuiTestHarness, path_bin: &Path) {
    let shell = h.home_path().join("omp-test-shell");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\n[ \"${{1-}}\" = -l ] && shift\nexport PATH={}:\"$PATH\"\nexec /bin/sh \"$@\"\n",
            sh_quote(path_bin)
        ),
    )
    .expect("write OMP test shell");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755))
            .expect("chmod OMP test shell");
    }
    h.set_env("SHELL", shell.to_str().expect("UTF-8 OMP test shell"));
}

fn launched_tmux_name(h: &TuiTestHarness, title: &str) -> String {
    let content = fs::read_to_string(sessions_path(h)).expect("read sessions.json");
    let sessions: Value = serde_json::from_str(&content).expect("parse sessions.json");
    let id = sessions
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["title"].as_str() == Some(title)))
        .and_then(|row| row["id"].as_str())
        .unwrap_or_else(|| panic!("no session titled {title:?}"));
    agent_of_empires::tmux::Session::generate_name(id, title)
}

fn tmux_environment_contains(h: &TuiTestHarness, title: &str, key: &str) -> bool {
    let output = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "show-environment",
            "-h",
            "-t",
            &launched_tmux_name(h, title),
        ])
        .output()
        .expect("show tmux environment");
    assert!(
        output.status.success(),
        "tmux show-environment failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let prefix = format!("{key}=");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with(&prefix))
}

fn tmux_pane_start_command(h: &TuiTestHarness, title: &str) -> String {
    let output = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "display-message",
            "-p",
            "-t",
            &launched_tmux_name(h, title),
            "#{pane_start_command}",
        ])
        .output()
        .expect("read tmux pane start command");
    assert!(
        output.status.success(),
        "tmux display-message failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn unset_tmux_environment(h: &TuiTestHarness, title: &str, key: &str) {
    let output = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "set-environment",
            "-h",
            "-u",
            "-t",
            &launched_tmux_name(h, title),
            key,
        ])
        .output()
        .expect("unset tmux environment");
    assert!(
        output.status.success(),
        "tmux set-environment -u failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_past_tmux_creation_second(h: &TuiTestHarness, title: &str) {
    let output = std::process::Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "display-message",
            "-p",
            "-t",
            &launched_tmux_name(h, title),
            "#{session_created}",
        ])
        .output()
        .expect("read tmux session creation time");
    assert!(
        output.status.success(),
        "tmux display-message failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let created_secs: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("tmux session_created must be epoch seconds");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs();
        if now_secs > created_secs {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting past tmux creation second {created_secs}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_for_path(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn wait_for_agent_session_id(h: &TuiTestHarness, title: &str, expected: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let actual = agent_session_id(h, title);
        if actual.as_deref() == Some(expected) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {title:?} to capture {expected}; last value was {actual:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn install_toggling_fake_omp(h: &mut TuiTestHarness, project: &Path, omp_store: &Path) -> PathBuf {
    let control = h.home_path().join("omp-toggle-control");
    fs::create_dir_all(&control).expect("create fake OMP control directory");
    let bin = h.install_path_command("omp");
    install_path_preserving_test_shell(h, &bin);
    let script = format!(
        "#!/bin/sh\n\
         control={control}\n\
         if [ ! -f \"$control/launched-first\" ]; then\n\
           : > \"$control/launched-first\"; slot=first; sid={first}\n\
         elif [ ! -f \"$control/launched-second\" ]; then\n\
           : > \"$control/launched-second\"; slot=second; sid={second}\n\
         else\n\
           slot=third; sid={third}\n\
         fi\n\
         injected_store=${{PI_CODING_AGENT_DIR-}}\n\
         printf '%s\\n' \"$injected_store\" > \"$control/pi-dir-$slot\"\n\
         printf '%s\\n' \"${{PI_CONFIG_DIR-}}\" > \"$control/config-dir-$slot\"\n\
         printf '%s\\n' \"$@\" > \"$control/args-$slot\"\n\
         received={store}\n\
         sessions_dir=\"$received/sessions/home-project\"\n\
         terminal_dir=\"$received/terminal-sessions\"\n\
         mkdir -p \"$sessions_dir\" \"$terminal_dir\"\n\
         session_path=\"$sessions_dir/2026-08-05T00-00-00-000Z_${{sid}}.jsonl\"\n\
         printf '{{\"type\":\"title\",\"v\":1,\"title\":\"Fake OMP capture\",\"source\":\"user\",\"updatedAt\":\"2026-08-05T00:00:00.000Z\",\"pad\":\"\"}}\\n' > \"$session_path\"\n\
         printf '{{\"type\":\"session\",\"version\":3,\"id\":\"%s\",\"timestamp\":\"2026-08-05T00:00:00.000Z\",\"cwd\":\"%s\"}}\\n' \"$sid\" {cwd} >> \"$session_path\"\n\
         tty_path=$(tty) || exit 1\n\
         terminal_id=$(printf '%s' \"${{tty_path#/dev/}}\" | tr '/' '-')\n\
         printf '%s\\n%s\\nfresh\\n' {cwd} \"$session_path\" > \"$terminal_dir/$terminal_id\"\n\
         : > \"$control/ready-$slot\"\n\
         exec sleep 300\n",
        control = sh_quote(&control),
        first = OMP_SID_FIRST,
        second = OMP_SID_SECOND,
        third = OMP_SID_THIRD,
        cwd = sh_quote(project),
        store = sh_quote(omp_store),
    );
    let script_path = bin.join("omp");
    fs::write(&script_path, script).expect("write fake omp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod omp");
    }
    control
}

fn write_stale_omp_session(store: &Path, old_project: &Path) -> PathBuf {
    let path = store.join(format!(
        "sessions/old-project/2020-01-01T00-00-00-000Z_{OMP_STALE_SID}.jsonl"
    ));
    fs::create_dir_all(path.parent().expect("stale session parent"))
        .expect("create stale OMP session directory");
    fs::write(
        &path,
        format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":\"Stale OMP session\",\"source\":\"user\",\"updatedAt\":\"2020-01-01T00:00:00.000Z\",\"pad\":\"\"}}\n\
             {{\"type\":\"session\",\"version\":3,\"id\":\"{OMP_STALE_SID}\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
            old_project.display()
        ),
    )
    .expect("write stale OMP session");
    path
}

fn install_reconstructing_fake_omp(
    h: &mut TuiTestHarness,
    metadata_store: &Path,
    legacy_store: &Path,
    project: &Path,
    old_project: &Path,
) {
    let bin = h.install_path_command("omp");
    install_path_preserving_test_shell(h, &bin);
    let control = h.home_path().join("omp-control");
    fs::create_dir_all(&control).expect("create fake OMP control directory");
    let metadata_stale = write_stale_omp_session(metadata_store, old_project);
    let legacy_stale = write_stale_omp_session(legacy_store, old_project);
    let script = format!(
        "#!/bin/sh\n\
         control={control}\n\
         if [ -f \"$control/launched\" ]; then\n\
           slot=legacy; sid={second}; stale={legacy_stale}; store={legacy_store}\n\
         else\n\
           : > \"$control/launched\"\n\
           slot=metadata; sid={first}; stale={metadata_stale}; store={metadata_store}\n\
         fi\n\
         printf '%s\\n' \"${{PI_CODING_AGENT_DIR-}}\" > \"$control/pi-dir-$slot\"\n\
         printf '%s\\n' \"${{PI_CONFIG_DIR-}}\" > \"$control/config-dir-$slot\"\n\
         sessions_dir=\"$store/sessions/home-project\"\n\
         terminal_dir=\"$store/terminal-sessions\"\n\
         mkdir -p \"$sessions_dir\" \"$terminal_dir\"\n\
         tty_path=$(tty) || exit 1\n\
         terminal_id=$(printf '%s' \"${{tty_path#/dev/}}\" | tr '/' '-')\n\
         breadcrumb=\"$terminal_dir/$terminal_id\"\n\
         printf '%s\\n%s\\n' {old_cwd} \"$stale\" > \"$breadcrumb.tmp\"\n\
         touch -t 202001010000 \"$breadcrumb.tmp\"\n\
         mv \"$breadcrumb.tmp\" \"$breadcrumb\"\n\
         : > \"$control/ready-$slot\"\n\
         while [ ! -f \"$control/release-$slot\" ]; do sleep 0.05; done\n\
         if [ \"$slot\" = metadata ]; then\n\
           printf '%s\\n%s\\n' {old_cwd} \"$stale\" > \"$breadcrumb.tmp\"\n\
         else\n\
           fresh=\"$sessions_dir/2026-08-05T00-00-00-000Z_${{sid}}.jsonl\"\n\
           printf '{{\"type\":\"title\",\"v\":1,\"title\":\"Reconstructed OMP session\",\"source\":\"user\",\"updatedAt\":\"2026-08-05T00:00:00.000Z\",\"pad\":\"\"}}\\n' > \"$fresh\"\n\
           printf '{{\"type\":\"session\",\"version\":3,\"id\":\"%s\",\"timestamp\":\"2026-08-05T00:00:00.000Z\",\"cwd\":\"%s\"}}\\n' \"$sid\" {cwd} >> \"$fresh\"\n\
           printf '%s\\n%s\\n' {cwd} \"$fresh\" > \"$breadcrumb.tmp\"\n\
         fi\n\
         mv \"$breadcrumb.tmp\" \"$breadcrumb\"\n\
         : > \"$control/switched-$slot\"\n\
         exec sleep 300\n",
        control = sh_quote(&control),
        metadata_stale = sh_quote(&metadata_stale),
        legacy_stale = sh_quote(&legacy_stale),
        metadata_store = sh_quote(metadata_store),
        legacy_store = sh_quote(legacy_store),
        first = OMP_SID_FIRST,
        second = OMP_SID_SECOND,
        old_cwd = sh_quote(old_project),
        cwd = sh_quote(project),
    );
    let script_path = bin.join("omp");
    fs::write(&script_path, script).expect("write reconstructing fake omp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod omp");
    }
}

#[test]
#[parallel]
fn omp_routing_restart_generation_and_same_cwd_pane_attribution_are_preserved() {
    require_tmux!();
    let mut h = new_harness("cli_sid_omp_terminal");
    let project = h.project_path();
    let omp_store = h.home_path().join("dotenv-omp-store");
    fs::create_dir_all(omp_store.join("sessions/decoy")).expect("create OMP store");
    write_project_omp_dotenv(&project, &omp_store);
    configure_fresh_restart_capture(&h);
    let control = install_toggling_fake_omp(&mut h, &project, &omp_store);

    fs::write(
        omp_store
            .join("sessions/decoy")
            .join(format!("2020-01-01T00-00-00-000Z_{OMP_STALE_SID}.jsonl")),
        format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":\"Decoy OMP session\",\"source\":\"user\",\"updatedAt\":\"2020-01-01T00:00:00.000Z\",\"pad\":\"\"}}\n\
             {{\"type\":\"session\",\"version\":3,\"id\":\"{OMP_STALE_SID}\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
            project.display()
        ),
    )
    .expect("write OMP decoy");

    for title in [OMP_TITLE_FIRST, OMP_TITLE_SECOND] {
        let add = h.run_cli(&[
            "add",
            project.to_str().unwrap(),
            "-c",
            "omp",
            "-t",
            title,
            "--extra-args=--thinking low",
        ]);
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let _stop_first = StopSessionOnDrop {
        h: &h,
        title: OMP_TITLE_FIRST,
    };
    let _stop_second = StopSessionOnDrop {
        h: &h,
        title: OMP_TITLE_SECOND,
    };

    let mut generations = Vec::new();
    for (operation, title, slot, expected) in [
        ("start", OMP_TITLE_FIRST, "first", OMP_SID_FIRST),
        ("restart", OMP_TITLE_FIRST, "second", OMP_SID_SECOND),
        ("start", OMP_TITLE_SECOND, "third", OMP_SID_THIRD),
    ] {
        let output = h.run_cli(&["session", operation, title]);
        assert!(
            output.status.success(),
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        wait_for_path(&control.join(format!("ready-{slot}")));
        assert_eq!(
            fs::read_to_string(control.join(format!("pi-dir-{slot}")))
                .expect("read PI_CODING_AGENT_DIR"),
            "\n",
            "{operation} must not inject resolved dotenv routing into the OMP process environment"
        );
        assert_eq!(
            fs::read_to_string(control.join(format!("config-dir-{slot}")))
                .expect("read PI_CONFIG_DIR"),
            "\n",
            "{operation} must not inject dotenv-expanded routing secrets"
        );
        assert!(
            !tmux_pane_start_command(&h, title).contains(OMP_ROUTING_SECRET),
            "{operation} must not persist dotenv-expanded routing secrets in pane argv"
        );
        assert_eq!(
            fs::read_to_string(control.join(format!("args-{slot}")))
                .expect("read fake OMP arguments"),
            "--thinking\nlow\n",
            "the benign extra_args must reach OMP unchanged"
        );
        wait_for_agent_session_id(&h, title, expected);
        let generation = omp_capture_generation(&h, title)
            .unwrap_or_else(|| panic!("{operation} must persist an OMP capture generation"));
        generations.push(generation);
    }
    // Restart and a fresh same-cwd launch must each mint their own durable
    // generation; a collision would let one pane's capture clobber another's.
    let distinct: std::collections::HashSet<_> = generations.iter().collect();
    assert_eq!(
        distinct.len(),
        generations.len(),
        "each same-cwd (re)launch must mint a distinct OMP capture generation: {generations:?}"
    );
    assert_eq!(
        agent_session_id(&h, OMP_TITLE_FIRST).as_deref(),
        Some(OMP_SID_SECOND),
        "the second same-cwd launch must not steal the restarted pane's SID2"
    );
}

#[test]
#[parallel]
fn omp_reconstruction_rejects_prelaunch_then_accepts_cross_project_and_backfills_legacy() {
    require_tmux!();
    let mut h = new_harness("cli_sid_omp_reconstruction");
    let project = h.project_path();
    let old_project = h.home_path().join("unrelated-old-project");
    fs::create_dir_all(&old_project).expect("create unrelated old project");
    let metadata_store = h.home_path().join("metadata-omp-store");
    let legacy_store = h.home_path().join("legacy-omp-store");
    write_project_omp_dotenv(&project, &metadata_store);
    install_reconstructing_fake_omp(
        &mut h,
        &metadata_store,
        &legacy_store,
        &project,
        &old_project,
    );

    let metadata_title = "CliSidOmpMetadataReconstructionE2E";
    let legacy_title = "CliSidOmpLegacyReconstructionE2E";
    for title in [metadata_title, legacy_title] {
        let add = h.run_cli(&["add", project.to_str().unwrap(), "-c", "omp", "-t", title]);
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let metadata_start = h.run_cli(&["session", "start", metadata_title]);
    assert!(
        metadata_start.status.success(),
        "metadata start failed: {}",
        String::from_utf8_lossy(&metadata_start.stderr)
    );
    let control = h.home_path().join("omp-control");
    wait_for_path(&control.join("ready-metadata"));
    assert_eq!(
        fs::read_to_string(control.join("pi-dir-metadata"))
            .expect("read metadata PI_CODING_AGENT_DIR"),
        "\n",
        "metadata launch must not inject resolved dotenv routing"
    );
    assert_eq!(
        fs::read_to_string(control.join("config-dir-metadata"))
            .expect("read metadata PI_CONFIG_DIR"),
        "\n",
        "metadata launch must not inject dotenv-expanded routing secrets"
    );
    assert!(
        !tmux_pane_start_command(&h, metadata_title).contains(OMP_ROUTING_SECRET),
        "metadata launch must not persist dotenv-expanded routing secrets in pane argv"
    );
    assert_eq!(
        agent_session_id(&h, metadata_title),
        None,
        "a pre-launch breadcrumb targeting an old session from another project must be rejected"
    );

    write_project_omp_dotenv(&project, &legacy_store);
    let legacy_start = h.run_cli(&["session", "start", legacy_title]);
    assert!(
        legacy_start.status.success(),
        "legacy start failed: {}",
        String::from_utf8_lossy(&legacy_start.stderr)
    );
    wait_for_path(&control.join("ready-legacy"));
    assert_eq!(
        fs::read_to_string(control.join("pi-dir-legacy")).expect("read legacy PI_CODING_AGENT_DIR"),
        "\n",
        "legacy launch must not inject newly resolved dotenv routing"
    );
    assert_eq!(
        fs::read_to_string(control.join("config-dir-legacy")).expect("read legacy PI_CONFIG_DIR"),
        "\n",
        "legacy launch must not inject dotenv-expanded routing secrets"
    );
    assert!(
        !tmux_pane_start_command(&h, legacy_title).contains(OMP_ROUTING_SECRET),
        "legacy launch must not persist dotenv-expanded routing secrets in pane argv"
    );
    assert_eq!(
        agent_session_id(&h, legacy_title),
        None,
        "the legacy pane must also reject its initial stale breadcrumb"
    );
    assert!(
        tmux_environment_contains(&h, legacy_title, OMP_CAPTURE_META_KEY),
        "new launches must persist the typed OMP capture metadata before legacy reconstruction"
    );
    unset_tmux_environment(&h, legacy_title, OMP_CAPTURE_META_KEY);
    unset_tmux_environment(&h, legacy_title, OMP_LAUNCH_ID_KEY);
    unset_tmux_environment(&h, legacy_title, OMP_CAPTURE_READY_KEY);
    clear_omp_capture_generation(&h, legacy_title);
    assert!(
        !tmux_environment_contains(&h, legacy_title, OMP_CAPTURE_META_KEY),
        "the legacy scenario requires capture metadata to be absent"
    );

    // Legacy reconstruction rounds `#{session_created}` up to the end of its
    // second. Release only after that boundary so the rewritten breadcrumb is
    // affirmative post-launch evidence rather than an ambiguous same-second write.
    wait_past_tmux_creation_second(&h, legacy_title);
    fs::write(control.join("release-metadata"), "").expect("release metadata fake OMP");
    fs::write(control.join("release-legacy"), "").expect("release legacy fake OMP");
    wait_for_path(&control.join("switched-metadata"));
    wait_for_path(&control.join("switched-legacy"));

    h.spawn_tui();
    let _stop_metadata = StopSessionOnDrop {
        h: &h,
        title: metadata_title,
    };
    let _stop_legacy = StopSessionOnDrop {
        h: &h,
        title: legacy_title,
    };
    wait_for_agent_session_id(&h, metadata_title, OMP_STALE_SID);
    wait_for_agent_session_id(&h, legacy_title, OMP_SID_SECOND);
    assert!(
        tmux_environment_contains(&h, legacy_title, OMP_CAPTURE_META_KEY),
        "legacy reconstruction must backfill typed OMP capture metadata"
    );
}

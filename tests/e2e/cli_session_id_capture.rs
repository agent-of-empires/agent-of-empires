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

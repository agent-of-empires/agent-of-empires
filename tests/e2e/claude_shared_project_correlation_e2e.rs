//! Full-stack e2e: two Claude sessions sharing one project keep identities
//! scoped to their own authoritative per-pane hook sidecars.
//!
//! Variant 1 proves an empty thread resolves from its sidecar. Variant 2 proves
//! shared transcript files are not an identity source: without a sidecar, the
//! session retains its launch-pinned UUID while its peer resolves independently.
//!
//! `claude_poll_fn` runs inside whichever process owns the session's poller,
//! and that process is the one that persists observations into `sessions.json`:
//! the TUI drains via `apply_session_id_updates`, which calls the shared
//! `drain_and_persist_session_ids` the daemon invokes directly. A `claude`
//! session's poller is created in `finalize_launch`;
//! the CLI process that launches the session exits right after, so its poller
//! dies with it. The native TUI is the long-lived host used here: on startup it
//! starts a poller for every already-live session it loads
//! (`HomeView::new`, `src/tui/home/lifecycle.rs`), one at a time, and its tick drains
//! each observation to disk.
//!
//! Sessions are created with `aoe add` and launched with `aoe session start`
//! (a deterministic, blocking CLI launch that, unlike `aoe add -l`, attaches no
//! controlling terminal) so both panes are already live when the TUI loads them;
//! the TUI then only starts pollers (never relaunches), which sidesteps the
//! concurrent startup-recovery cascade entirely. Each session is minted a fresh
//! random Claude UUID at launch, DISTINCT from the shim UUID the hook publishes,
//! so the only way `sessions.json` can end up holding the shim UUID is the
//! mechanism under test; a revert leaves the launch-minted id (or `None`) and
//! the positive assertion fails.
//!
//! Daemon-free (like the sibling `resume_fallback` e2e), so no feature gate.
//! Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- claude_shared_project_correlation --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

// Shim UUIDs, one per session. These are what each session's own hook writes,
// and what the poller must correlate back to that session.
const UUID_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const UUID_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

// Deadline and cadence for the correlation wait, and the deadline for a shim to
// publish its files.
const CORRELATE_DEADLINE: Duration = Duration::from_secs(30);
const CORRELATE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHIM_DEADLINE: Duration = Duration::from_secs(10);
const SHIM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One session's authoritative sidecar and untrusted transcript fixtures.
struct SessionSpec {
    title: &'static str,
    sidecar: bool,
    jsonl: bool,
    uuid: &'static str,
    jsonl_mtime_secs_ago: Option<u64>,
}

/// Encode a project path the way `encode_claude_project_path` does: every
/// character that is not ASCII-alphanumeric or `-` becomes `-`.
fn encode_project_path(p: &str) -> String {
    p.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Parse the `  ID:      <id>` line that `aoe add` prints on success.
fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// Install a bespoke `claude` shim on PATH: `install_path_command` prepends a
/// path-bin dir (ahead of the harness's default exit-0 `claude` stub) holding an
/// exit-0 placeholder, which this overwrites with the real shim. On launch it
/// publishes its own session UUID exactly as a real Claude session would:
/// writing the sidecar through the real `aoe __extract-session-id` subcommand
/// (never hand-writing the file, so the guard-anchored writer stays under test)
/// and/or dropping a `<uuid>.jsonl` transcript. It then `exec sleep` so the pane
/// stays live, which both keeps the tmux session visible to
/// `build_exclusion_set`'s peer scan and holds the poller open.
fn install_claude_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("claude");
    let aoe = env!("CARGO_BIN_EXE_aoe");
    // If AOE_INSTANCE_ID is unset the launch-env contract broke; fail loudly
    // (exit 3 + marker) rather than silently pass. The test writes the role file
    // before launching, so the shim normally finds it on the first check; the
    // bounded spin-wait is a safety net.
    let script = format!(
        r#"#!/bin/sh
if [ -z "$AOE_INSTANCE_ID" ]; then
  echo "missing AOE_INSTANCE_ID" > "$HOME/shim-missing-instance.marker"
  exit 3
fi
ROLE="$HOME/roles/$AOE_INSTANCE_ID"
i=0
while [ ! -f "$ROLE" ] && [ "$i" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
if [ ! -f "$ROLE" ]; then
  echo "missing role for $AOE_INSTANCE_ID" > "$HOME/shim-missing-role.marker"
  exec sleep 600
fi
. "$ROLE"
if [ "$SIDECAR" = "yes" ]; then
  printf '{{"session_id":"%s"}}' "$UUID" | "{aoe}" __extract-session-id
fi
if [ "$JSONL" = "yes" ]; then
  mkdir -p "$JSONL_DIR"
  printf '{{}}\n' > "$JSONL_DIR/$UUID.jsonl"
  if [ -n "$MTIME_REF" ]; then
    touch -r "$MTIME_REF" "$JSONL_DIR/$UUID.jsonl"
  fi
fi
# uuid-map is written LAST so its presence is a true "role fully applied"
# barrier for wait_for_shim: its sidecar and/or jsonl (whichever the role
# enabled) are already on disk.
mkdir -p "$HOME/uuid-map"
printf '%s' "$UUID" > "$HOME/uuid-map/$AOE_INSTANCE_ID"
exec sleep 600
"#,
    );
    let path = bin.join("claude");
    std::fs::write(&path, &script).expect("write claude shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

/// Create a zero-byte reference file whose mtime is `secs_ago` in the past, for
/// the shim to `touch -r` against. Setting the time from Rust keeps the jsonl
/// ordering deterministic without any sleep, and `touch -r` is portable across
/// macOS and Linux.
fn make_mtime_ref(h: &TuiTestHarness, name: &str, secs_ago: u64) -> PathBuf {
    let path = h.home_path().join(name);
    let when = SystemTime::now() - Duration::from_secs(secs_ago);
    std::fs::File::create(&path)
        .expect("create mtime ref")
        .set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set mtime ref");
    path
}

/// Write the shim's role file for `instance_id`.
fn write_role(
    h: &TuiTestHarness,
    instance_id: &str,
    spec: &SessionSpec,
    jsonl_dir: &Path,
    mtime_ref: Option<&Path>,
) {
    let roles = h.home_path().join("roles");
    std::fs::create_dir_all(&roles).expect("create roles dir");
    let mtime_ref = mtime_ref
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // The shim sources this file with `. "$ROLE"`. Values are controlled (fixed
    // UUIDs, yes/no, and tempdir paths with no shell metacharacters), so
    // single-quoting is sufficient and no embedded-quote escaping is needed.
    let body = format!(
        "SIDECAR='{}'\nJSONL='{}'\nUUID='{}'\nJSONL_DIR='{}'\nMTIME_REF='{}'\n",
        if spec.sidecar { "yes" } else { "no" },
        if spec.jsonl { "yes" } else { "no" },
        spec.uuid,
        jsonl_dir.display(),
        mtime_ref,
    );
    std::fs::write(roles.join(instance_id), body).expect("write role file");
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

/// Read sessions.json while the poller host may be rewriting it.
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

/// Wait until both persisted identities match their expected authoritative
/// values across two consecutive reads.
fn await_expected_ids(
    h: &TuiTestHarness,
    id_a: &str,
    expected_a: &str,
    id_b: &str,
    expected_b: &str,
) {
    let deadline = Instant::now() + CORRELATE_DEADLINE;
    let mut consecutive_ok = 0;
    loop {
        let sessions = read_sessions(h);
        let a = agent_session_id_of(&sessions, id_a);
        let b = agent_session_id_of(&sessions, id_b);
        if a.as_deref() == Some(expected_a) && b.as_deref() == Some(expected_b) {
            consecutive_ok += 1;
            if consecutive_ok >= 2 {
                return;
            }
        } else {
            consecutive_ok = 0;
        }
        if Instant::now() >= deadline {
            let shim_no_inst = h.home_path().join("shim-missing-instance.marker").exists();
            let shim_no_role = h.home_path().join("shim-missing-role.marker").exists();
            let tmux_ls = std::process::Command::new("tmux")
                .arg("-S")
                .arg(h.home_path().join("tmux.sock"))
                .arg("ls")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            let debug_log = std::fs::read_to_string(app_dir_in(h.home_path()).join("debug.log"))
                .unwrap_or_default();
            let dbg_tail = debug_log
                .lines()
                .filter(|line| {
                    line.contains("session.sync")
                        || line.contains("session.capture")
                        || line.contains("session.store")
                })
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "session ids did not stabilize within {CORRELATE_DEADLINE:?}.\n\
                 expected: [{id_a}]={expected_a}, [{id_b}]={expected_b}\n\
                 observed: [{id_a}]={a:?}, [{id_b}]={b:?}\n\
                 AOE_INSTANCE_ID-missing marker: {shim_no_inst}, role-missing marker: {shim_no_role}\n\
                 tmux sessions:\n{tmux_ls}\n\
                 debug.log (sync/capture/store):\n{dbg_tail}\n\
                 sessions.json:\n{}",
                serde_json::to_string_pretty(&sessions).unwrap_or_default(),
            );
        }
        std::thread::sleep(CORRELATE_POLL_INTERVAL);
    }
}

/// Assert the shim actually ran with the expected instance-to-UUID mapping
/// (proves the launch env carried `AOE_INSTANCE_ID` and the shim executed its
/// role, not just that the poller happened to persist the right string).
fn assert_shim_recorded(h: &TuiTestHarness, instance_id: &str, uuid: &str) {
    let mapped = std::fs::read_to_string(h.home_path().join("uuid-map").join(instance_id))
        .unwrap_or_else(|e| panic!("shim never recorded uuid-map for {instance_id}: {e}"));
    assert_eq!(
        mapped.trim(),
        uuid,
        "shim recorded the wrong UUID for {instance_id}"
    );
}

/// Remove the per-instance hook directories on drop. The hook base
/// (`/tmp/aoe-hooks-<euid>/`) lives outside `$HOME`, so the harness's tempdir
/// teardown never sweeps it. Instance ids are unique per run, so this cannot
/// collide with another test.
struct HookDirCleanup {
    euid: String,
    instance_ids: Vec<String>,
}

impl HookDirCleanup {
    fn new(instance_ids: Vec<String>) -> Self {
        let euid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        Self { euid, instance_ids }
    }
}

impl Drop for HookDirCleanup {
    fn drop(&mut self) {
        if self.euid.is_empty() {
            return;
        }
        for id in &self.instance_ids {
            let _ = std::fs::remove_dir_all(format!("/tmp/aoe-hooks-{}/{}", self.euid, id));
        }
    }
}

/// Block until the shim for `instance_id` has recorded its uuid-map entry,
/// proving it ran its role (wrote the sidecar and/or jsonl) before the poller
/// host starts observing.
fn wait_for_shim(h: &TuiTestHarness, instance_id: &str) {
    let deadline = Instant::now() + SHIM_DEADLINE;
    while Instant::now() < deadline {
        if h.home_path().join("uuid-map").join(instance_id).exists() {
            return;
        }
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }
    let missing_inst = h.home_path().join("shim-missing-instance.marker").exists();
    let missing_role = h.home_path().join("shim-missing-role.marker").exists();
    panic!(
        "shim for {instance_id} never wrote its uuid-map entry within {SHIM_DEADLINE:?} \
         (AOE_INSTANCE_ID-missing marker: {missing_inst}, role-missing marker: {missing_role})"
    );
}

/// Create a session with `aoe add` (no launch); returns the instance id.
fn add_session(h: &TuiTestHarness, project: &str, spec: &SessionSpec) -> String {
    let add = h.run_cli(&["add", project, "-t", spec.title, "-c", "claude"]);
    assert!(
        add.status.success(),
        "aoe add {} failed.\nstdout: {}\nstderr: {}",
        spec.title,
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    parse_session_id(&String::from_utf8_lossy(&add.stdout))
}

/// Launch a session's tmux pane with `aoe session start` (a deterministic,
/// blocking CLI launch that, unlike `aoe add -l`, does not try to attach a
/// controlling terminal). The role must already be on disk so the shim finds it
/// on its first check.
fn launch_session(h: &TuiTestHarness, instance_id: &str, title: &str) {
    let start = h.run_cli(&["session", "start", instance_id]);
    assert!(
        start.status.success(),
        "aoe session start {} failed.\nstdout: {}\nstderr: {}",
        title,
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
}

/// Launch two Claude sessions on the same project, then verify each keeps the
/// identity permitted by its authoritative sidecar state.
fn run_shared_project_correlation(test_name: &str, spec_a: SessionSpec, spec_b: SessionSpec) {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp(test_name);
    let claude_home = h.home_path().join(".claude");
    h.set_env("CLAUDE_CONFIG_DIR", &claude_home.display().to_string());
    install_claude_shim(&mut h);

    // Per-spec mtime reference files (only for specs that pin a jsonl mtime).
    let ref_a = spec_a
        .jsonl_mtime_secs_ago
        .map(|secs| make_mtime_ref(&h, "mtime-ref-a", secs));
    let ref_b = spec_b
        .jsonl_mtime_secs_ago
        .map(|secs| make_mtime_ref(&h, "mtime-ref-b", secs));

    let project = h.project_path();
    let project_str = project.to_str().expect("utf8 project path").to_string();
    // The poller scans `<claude_home>/projects/<encoded-canonical-cwd>/`, where
    // the cwd is the canonicalized project path (on macOS `/tmp` resolves to
    // `/private/tmp`). Mirror that so the shim drops the jsonl where the poller
    // looks.
    let canonical = std::fs::canonicalize(&project).unwrap_or_else(|_| project.clone());
    let jsonl_dir = claude_home
        .join("projects")
        .join(encode_project_path(&canonical.to_string_lossy()));

    // Add both sessions (no launch) so their ids and sessions.json rows exist,
    // and register cleanup BEFORE any launch can fail so a partial launch cannot
    // leak a hook dir.
    let id_a = add_session(&h, &project_str, &spec_a);
    let id_b = add_session(&h, &project_str, &spec_b);
    let _hook_cleanup = HookDirCleanup::new(vec![id_a.clone(), id_b.clone()]);

    // Write both roles BEFORE launching, so each shim finds its role on its first
    // check with no dependency on the other session's launch time.
    write_role(&h, &id_a, &spec_a, &jsonl_dir, ref_a.as_deref());
    write_role(&h, &id_b, &spec_b, &jsonl_dir, ref_b.as_deref());

    launch_session(&h, &id_a, spec_a.title);
    launch_session(&h, &id_b, spec_b.title);

    wait_for_shim(&h, &id_a);
    wait_for_shim(&h, &id_b);

    let launched = read_sessions(&h);
    let expected_a = if spec_a.sidecar {
        spec_a.uuid.to_string()
    } else {
        agent_session_id_of(&launched, &id_a).expect("Claude launch must pin an identity")
    };
    let expected_b = if spec_b.sidecar {
        spec_b.uuid.to_string()
    } else {
        agent_session_id_of(&launched, &id_b).expect("Claude launch must pin an identity")
    };
    if !spec_a.sidecar {
        assert_ne!(
            expected_a, spec_a.uuid,
            "transcript UUID must not be adopted"
        );
        assert_ne!(
            expected_a, spec_b.uuid,
            "peer transcript UUID must not be adopted"
        );
    }

    h.spawn_tui();
    h.wait_for_ready();

    await_expected_ids(&h, &id_a, &expected_a, &id_b, &expected_b);
    assert_shim_recorded(&h, &id_a, spec_a.uuid);
    assert_shim_recorded(&h, &id_b, spec_b.uuid);
}

/// An empty thread has no transcript, so only its sidecar can replace the
/// launch-pinned UUID with the hook-reported UUID.
#[test]
#[parallel]
fn claude_shared_project_correlation_variant1_sidecar_authoritative() {
    run_shared_project_correlation(
        "claude_shared_v1",
        SessionSpec {
            title: "shared-A",
            sidecar: true,
            jsonl: false,
            uuid: UUID_A,
            jsonl_mtime_secs_ago: None,
        },
        SessionSpec {
            title: "shared-B",
            sidecar: true,
            jsonl: true,
            uuid: UUID_B,
            jsonl_mtime_secs_ago: None,
        },
    );
}

/// A session without a sidecar keeps its launch-pinned UUID even when its own
/// and a peer's transcript files exist. The peer still resolves from its own
/// sidecar.
#[test]
#[parallel]
fn claude_shared_project_correlation_variant2_filesystem_scan_rejected() {
    run_shared_project_correlation(
        "claude_shared_v2",
        SessionSpec {
            title: "shared-A",
            sidecar: false,
            jsonl: true,
            uuid: UUID_A,
            jsonl_mtime_secs_ago: Some(60),
        },
        SessionSpec {
            title: "shared-B",
            sidecar: true,
            jsonl: true,
            uuid: UUID_B,
            jsonl_mtime_secs_ago: Some(20),
        },
    );
}

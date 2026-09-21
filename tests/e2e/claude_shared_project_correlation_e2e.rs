//! Full-stack e2e: two Claude sessions sharing one project keep identities
//! scoped to their own authoritative per-pane hook sidecars. Without a sidecar
//! a session keeps its launch-pinned UUID: shared transcript files are not an
//! identity source.
//!
//! The native TUI is the poller host (a CLI launcher exits and takes its poller
//! with it), so sessions are created with `aoe add`, launched with
//! `aoe session start`, and only then loaded by the TUI, which starts pollers
//! without relaunching. Each launch mints a UUID distinct from the shim's, so a
//! revert leaves the launch-minted id and the positive assertion fails.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serial_test::parallel;

use crate::harness::{
    agent_session_id_of, app_dir_in, require_tmux, write_executable, TuiTestHarness,
};

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

/// A `claude` shim that publishes its UUID the way a real session would: the
/// sidecar through the real `aoe __extract-session-id` (so the guard-anchored
/// writer stays under test) and/or a `<uuid>.jsonl` transcript, then stays
/// alive so the pane and its poller do too.
fn install_claude_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("claude");
    let aoe = env!("CARGO_BIN_EXE_aoe");
    // A missing AOE_INSTANCE_ID or role means the launch-env contract broke, so
    // the shim leaves a marker rather than passing silently.
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
    write_executable(&bin.join("claude"), &script);
}

/// Reference file `secs_ago` in the past for the shim to `touch -r`, which
/// orders the transcripts deterministically without a sleep.
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
    // Sourced by the shim; the values carry no shell metacharacters.
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
        let sessions = h.try_read_sessions();
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

/// The shim ran with the expected instance-to-UUID mapping, so the persisted id
/// came from the mechanism under test rather than a coincidence.
fn assert_shim_recorded(h: &TuiTestHarness, instance_id: &str, uuid: &str) {
    let mapped = std::fs::read_to_string(h.home_path().join("uuid-map").join(instance_id))
        .unwrap_or_else(|e| panic!("shim never recorded uuid-map for {instance_id}: {e}"));
    assert_eq!(
        mapped.trim(),
        uuid,
        "shim recorded the wrong UUID for {instance_id}"
    );
}

/// The hook base (`/tmp/aoe-hooks-<euid>/`) lives outside `$HOME`, so the
/// harness tempdir teardown never sweeps it.
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

/// Block until the shim recorded its uuid-map entry, which it writes last, so
/// its sidecar and jsonl are already on disk.
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
    // The poller scans `<claude_home>/projects/<encoded canonical cwd>/`.
    let canonical = std::fs::canonicalize(&project).unwrap_or_else(|_| project.clone());
    let jsonl_dir = claude_home
        .join("projects")
        .join(encode_project_path(&canonical.to_string_lossy()));

    // Cleanup is registered before any launch so a partial launch leaks no hook dir.
    let id_a = h.add_session(&[&project_str, "-t", spec_a.title, "-c", "claude"]);
    let id_b = h.add_session(&[&project_str, "-t", spec_b.title, "-c", "claude"]);
    let _hook_cleanup = HookDirCleanup::new(vec![id_a.clone(), id_b.clone()]);

    // Both roles land before either launch, so each shim finds its own on the
    // first check regardless of launch order.
    write_role(&h, &id_a, &spec_a, &jsonl_dir, ref_a.as_deref());
    write_role(&h, &id_b, &spec_b, &jsonl_dir, ref_b.as_deref());

    // `session start` is a blocking launch that attaches no terminal.
    h.run_cli_ok(&["session", "start", &id_a]);
    h.run_cli_ok(&["session", "start", &id_b]);

    wait_for_shim(&h, &id_a);
    wait_for_shim(&h, &id_b);

    let launched = h.try_read_sessions();
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

//! Full-stack e2e: a host Pi pane that could not be pinned captures its
//! conversation once, and nothing in the shared store may replace it after
//! that.
//!
//! A binary without `--session-id` (pi before 0.76.0, a command override, a
//! profile fronting another `pi`) launches unpinned and learns its id from the
//! floored poller. That first capture is the pane's own; every later
//! observation is a guess against a directory shared by every session here.
//!
//! Teeth: the shim advertises no `--session-id` and mints its own conversation
//! as pi does. Once anchored, the test drops in a conversation no AoE row
//! claims, which `session::sync`'s owner check cannot reject.
//!
//! Daemon-free, so no feature gate. Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- pi_unpinned_capture_is_final --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// The conversation the shim mints for itself, standing in for the id pi
/// would write on its first message.
const OWN_SESSION: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
/// A conversation belonging to no AoE session, dropped into the shared store
/// once the pane is anchored.
const FOREIGN_SESSION: &str = "ffffffff-2222-4222-8222-ffffffffffff";

const CAPTURE_DEADLINE: Duration = Duration::from_secs(30);
const OBSERVE_WINDOW: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A `pi` stub whose `--help` predates `--session-id`, so AoE launches it
/// unpinned, and which mints its own conversation into the shared store.
fn install_old_pi_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("pi");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --help)
    echo "Options:"
    echo "  --session <path|id>            Use specific session file or partial UUID"
    exit 0
    ;;
esac
encoded="--$(printf '%s' "${{PWD#/}}" | sed 's![/\\:]!-!g')--"
dir="$PI_CODING_AGENT_DIR/sessions/$encoded"
mkdir -p "$dir"
printf '{{"type":"session","id":"%s","cwd":"%s"}}\n' "{OWN_SESSION}" "$PWD" \
  > "$dir/20260101T000000_{OWN_SESSION}.jsonl"
exec sleep 600
"#
    );
    let path = bin.join("pi");
    std::fs::write(&path, script).expect("write pi shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn agent_session_id_of(h: &TuiTestHarness, instance_id: &str) -> Option<String> {
    let content = std::fs::read_to_string(sessions_path(h)).unwrap_or_default();
    let sessions: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
    sessions
        .as_array()?
        .iter()
        .find(|r| r["id"].as_str() == Some(instance_id))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("`aoe add` printed no ID line"))
}

fn run_cli(h: &TuiTestHarness, args: &[&str]) -> String {
    let out = h.run_cli(args);
    assert!(
        out.status.success(),
        "aoe CLI call failed.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[parallel]
fn pi_unpinned_capture_is_final() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("pi_unpinned_capture_is_final");
    let pi_home = h.home_path().join(".pi/agent");
    std::fs::create_dir_all(&pi_home).expect("mkdir pi home");
    h.set_env("PI_CODING_AGENT_DIR", &pi_home.display().to_string());
    install_old_pi_shim(&mut h);

    let project = h.home_path().join("unpinnable-project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let project_arg = project.to_str().expect("utf8 project");

    let instance = parse_session_id(&run_cli(
        &h,
        &["add", project_arg, "-t", "pi-unpinned", "-c", "pi"],
    ));
    run_cli(&h, &["session", "start", &instance]);

    // A live TUI drains the poller, which is the only way an unpinned pane
    // learns its conversation.
    h.spawn_tui();
    h.wait_for_ready();

    let deadline = Instant::now() + CAPTURE_DEADLINE;
    loop {
        if agent_session_id_of(&h, &instance).as_deref() == Some(OWN_SESSION) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unpinned pane never captured its own conversation within \
             {CAPTURE_DEADLINE:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }

    // Anchored. Now a conversation no row claims appears in the same
    // directory, newer than the pane's own.
    let dir = pi_home.join("sessions").join(format!(
        "--{}--",
        project
            .to_string_lossy()
            .strip_prefix('/')
            .unwrap_or_default()
            .replace(['/', '\\', ':'], "-")
    ));
    std::fs::create_dir_all(&dir).expect("mkdir session dir");
    std::fs::write(
        dir.join(format!("20260101T000000_{FOREIGN_SESSION}.jsonl")),
        format!(
            r#"{{"type":"session","id":"{FOREIGN_SESSION}","cwd":"{}"}}"#,
            project.to_string_lossy()
        ),
    )
    .expect("write foreign session");

    let deadline = Instant::now() + OBSERVE_WINDOW;
    while Instant::now() < deadline {
        assert_eq!(
            agent_session_id_of(&h, &instance).as_deref(),
            Some(OWN_SESSION),
            "the captured conversation was replaced by an unowned one"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

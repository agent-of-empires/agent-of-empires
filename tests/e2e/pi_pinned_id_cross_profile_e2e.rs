//! Full-stack e2e: a pinned Pi conversation survives a co-located peer in
//! ANOTHER profile that writes after this pane launched and then stops.
//!
//! Two shapes from #3576. A peer in another profile is refused by
//! `session::sync`'s `sid_owners` check, so that half pins existing behavior.
//! A conversation no row claims is the shape no ownership check can cover, and
//! that half fails without the poller gate.
//!
//! The shim writes a real `.jsonl` per launch and the pinned session runs
//! under a live TUI, which is what keeps its poller alive; a CLI one-shot
//! exits first and never polls again.
//!
//! Daemon-free, so no feature gate. Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- pi_pinned_id_cross_profile --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

const PEER_PROFILE: &str = "peer";
/// Long enough for several poll ticks (the poller starts at a 2s interval),
/// so a restored scan has every chance to adopt the peer's conversation.
const OBSERVE_WINDOW: Duration = Duration::from_secs(8);
const SHIM_DEADLINE: Duration = Duration::from_secs(20);
const SHIM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A `pi` stub that answers the capability probe and, unlike the stub in
/// `pi_pinned_session_id_e2e`, writes a real session file for the id it was
/// pinned with: this test needs the peer's conversation to actually be the
/// newest thing in the shared store.
fn install_pi_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("pi");
    let script = r#"#!/bin/sh
case "$1" in
  --help)
    echo "Options:"
    echo "  --session <path|id>            Use specific session file or partial UUID"
    echo "  --session-id <id>              Use exact project session ID, creating it if missing"
    exit 0
    ;;
esac
sid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id|--session) sid="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n' "$sid" >> "$HOME/pi-launches"
if [ -n "$sid" ]; then
  # Mirror pi's own layout: <store>/sessions/<cwd with / \ : mapped to ->/.
  encoded="--$(printf '%s' "${PWD#/}" | sed 's![/\\:]!-!g')--"
  dir="$PI_CODING_AGENT_DIR/sessions/$encoded"
  mkdir -p "$dir"
  printf '{"type":"session","id":"%s","cwd":"%s"}\n' "$sid" "$PWD" \
    > "$dir/20260101T000000_$sid.jsonl"
fi
exec sleep 600
"#;
    let path = bin.join("pi");
    std::fs::write(&path, script).expect("write pi shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

fn sessions_path(h: &TuiTestHarness, profile: &str) -> PathBuf {
    app_dir_in(h.home_path())
        .join("profiles")
        .join(profile)
        .join("sessions.json")
}

fn agent_session_id_of(h: &TuiTestHarness, profile: &str, instance_id: &str) -> Option<String> {
    let content = std::fs::read_to_string(sessions_path(h, profile)).unwrap_or_default();
    let sessions: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
    sessions
        .as_array()?
        .iter()
        .find(|r| r["id"].as_str() == Some(instance_id))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

/// Block until the shim has recorded `expected` launches, returning the ids
/// it was pinned with, oldest first.
fn wait_for_launches(h: &TuiTestHarness, expected: usize) -> Vec<String> {
    let path = h.home_path().join("pi-launches");
    let deadline = Instant::now() + SHIM_DEADLINE;
    loop {
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        if lines.len() >= expected {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "shim recorded {} of {expected} launches within {SHIM_DEADLINE:?}: {lines:#?}",
            lines.len()
        );
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }
}

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("`aoe add` printed no ID line"))
}

/// Run an `aoe` CLI command, in `profile` when given.
fn run_cli(h: &TuiTestHarness, profile: Option<&str>, args: &[&str]) -> String {
    let mut full: Vec<&str> = Vec::new();
    if let Some(profile) = profile {
        full.extend_from_slice(&["--profile", profile]);
    }
    full.extend_from_slice(args);
    let out = h.run_cli(&full);
    assert!(
        out.status.success(),
        "aoe CLI call failed.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[parallel]
fn pi_pinned_id_cross_profile() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("pi_pinned_id_cross_profile");
    // One Pi home for both profiles: the store is keyed by cwd, not by AoE
    // profile, which is the whole premise of the race.
    let pi_home = h.home_path().join(".pi/agent");
    std::fs::create_dir_all(&pi_home).expect("mkdir pi home");
    h.set_env("PI_CODING_AGENT_DIR", &pi_home.display().to_string());
    install_pi_shim(&mut h);

    let project = h.home_path().join("shared-project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let project_arg = project.to_str().expect("utf8 project");

    // The pinned session, in the default profile.
    let mine = parse_session_id(&run_cli(
        &h,
        None,
        &["add", project_arg, "-t", "pi-mine", "-c", "pi"],
    ));
    run_cli(&h, None, &["session", "start", &mine]);
    let pinned = wait_for_launches(&h, 1).remove(0);
    assert!(!pinned.is_empty(), "the launch carried no pinned id");
    assert_eq!(
        agent_session_id_of(&h, "default", &mine).as_deref(),
        Some(&*pinned)
    );

    // A live TUI keeps this session's poller alive across the peer's
    // lifetime. Without it the CLI one-shot has already exited and nothing
    // would be polling when the peer stops.
    h.spawn_tui();
    h.wait_for_ready();

    // The peer: another profile, same directory, same Pi home. It launches
    // after us, so its `.jsonl` is both newer than ours and inside our floor.
    let peer = parse_session_id(&run_cli(
        &h,
        Some(PEER_PROFILE),
        &["add", project_arg, "-t", "pi-peer", "-c", "pi"],
    ));
    run_cli(&h, Some(PEER_PROFILE), &["session", "start", &peer]);
    let peer_pinned = wait_for_launches(&h, 2).remove(1);
    assert_ne!(peer_pinned, pinned, "peer must pin its own conversation");

    // Stopping the peer removes the tmux ownership that was hiding its id,
    // and its profile is not the one our exclusion reads.
    run_cli(&h, Some(PEER_PROFILE), &["session", "stop", &peer]);

    // Nothing may retarget us onto the peer's conversation.
    let deadline = Instant::now() + OBSERVE_WINDOW;
    while Instant::now() < deadline {
        let observed = agent_session_id_of(&h, "default", &mine);
        assert_eq!(
            observed.as_deref(),
            Some(&*pinned),
            "the pinned conversation was retargeted (peer id: {peer_pinned})"
        );
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }

    // Second shape, the one no ownership check can cover: a conversation with
    // no AoE row at all, as a `pi` the user ran by hand in this directory
    // leaves behind. `sid_owners` in `session::sync` rejects a peer's id
    // because a row claims it; nothing claims this one, so only refusing to
    // scan keeps the pin.
    let foreign_dir = pi_home.join("sessions").join(format!(
        "--{}--",
        project
            .to_string_lossy()
            .strip_prefix('/')
            .unwrap_or_default()
            .replace(['/', '\\', ':'], "-")
    ));
    std::fs::create_dir_all(&foreign_dir).expect("mkdir foreign session dir");
    let foreign = "ffffffff-0000-4000-8000-ffffffffffff";
    std::fs::write(
        foreign_dir.join(format!("20260101T000000_{foreign}.jsonl")),
        format!(
            r#"{{"type":"session","id":"{foreign}","cwd":"{}"}}"#,
            project.to_string_lossy()
        ),
    )
    .expect("write foreign session");

    let deadline = Instant::now() + OBSERVE_WINDOW;
    while Instant::now() < deadline {
        assert_eq!(
            agent_session_id_of(&h, "default", &mine).as_deref(),
            Some(&*pinned),
            "an unowned conversation in the shared store was adopted"
        );
        std::thread::sleep(SHIM_POLL_INTERVAL);
    }

    // And the restart still resumes ours, not the peer's.
    run_cli(&h, None, &["session", "stop", &mine]);
    run_cli(&h, None, &["session", "start", &mine]);
    let relaunch = wait_for_launches(&h, 3).remove(2);
    assert_eq!(
        relaunch, pinned,
        "the restart must resume the pinned conversation, not the peer's"
    );
    assert_eq!(
        agent_session_id_of(&h, "default", &mine).as_deref(),
        Some(&*pinned)
    );
}

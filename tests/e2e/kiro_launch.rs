//! End-to-end coverage for how a Kiro session is launched.
//!
//! Kiro's interactive flags (`--trust-all-tools`, `--agent`, `--resume-id`)
//! live on the `kiro-cli chat` subcommand, not the top-level binary. AoE used
//! to launch bare `kiro-cli` plus the yolo flag, so YOLO mode produced
//! `kiro-cli --trust-all-tools`, which the real CLI rejects with
//! `error: unexpected argument '--trust-all-tools' found`.
//!
//! These tests drive the full `aoe add --launch` path and assert on the command
//! tmux was actually told to run (`pane_start_command`), so a regression in
//! launch-command construction is caught at the session-launch layer, not just
//! in the `build_host_command` unit tests. We read the command tmux recorded
//! rather than executing a fake `kiro-cli`: aoe wraps the launch in a login
//! shell (`sh -lc`) that re-resolves `kiro-cli` from the real PATH, so a stub
//! would be shadowed; `pane_start_command` captures the exact intended command
//! regardless of whether the binary is installed.

use crate::harness::{require_tmux, TuiTestHarness};
use serde_json::Value;
use serial_test::serial;
use std::process::Command;

/// Kills its tmux session when dropped, so a panicking assertion in the test
/// body still tears the real session down (the default tmux server is shared
/// across runs, so a leak would accumulate stale sessions).
struct TmuxSessionGuard(String);

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.0])
            .output();
    }
}

/// The tmux session name aoe derives for the session titled `title`
/// (`<SESSION_PREFIX><title>_<id[..8]>`). Looks the session up by title rather
/// than assuming a position, and panics with a clear message if it is absent,
/// so a launch that never persisted a session fails here rather than as a
/// downstream tmux lookup miss.
fn launched_tmux_name(h: &TuiTestHarness, title: &str) -> String {
    let path = crate::harness::app_dir_in(h.home_path())
        .join("profiles")
        .join("default")
        .join("sessions.json");
    let sessions: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| panic!("no sessions.json at {} after launch", path.display()));
    let id = sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .and_then(|s| s["id"].as_str())
        .unwrap_or_else(|| panic!("no session titled '{title}' in {}", path.display()));
    let truncated = &id[..8.min(id.len())];
    format!(
        "{}{}_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        title,
        truncated
    )
}

/// The command tmux was told to run for the session's pane. This is the launch
/// command aoe built, captured before (and independent of) execution.
fn pane_start_command(session: &str) -> String {
    let out = Command::new("tmux")
        .args(["list-panes", "-t", session, "-F", "#{pane_start_command}"])
        .output()
        .expect("tmux list-panes");
    assert!(
        out.status.success(),
        "tmux list-panes failed for {session}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run `aoe add --launch ...` for a kiro session and return the command tmux
/// was told to run. `--launch` creates the tmux session and then attempts a
/// foreground attach, which fails under the test's non-TTY stdio; that attach
/// failure is expected and irrelevant here, so the exit status is not asserted.
/// The session (and its recorded pane command) is created regardless, and
/// `launched_tmux_name` fails loudly if it wasn't. The returned guard kills the
/// session when the caller's scope ends, including on assertion panic.
fn launch_kiro_and_read_command(
    h: &mut TuiTestHarness,
    title: &str,
    extra: &[&str],
) -> (String, TmuxSessionGuard) {
    // `aoe add --tool kiro` verifies `kiro-cli` is on PATH before persisting the
    // session, so without a stub it bails (and never writes sessions.json) in
    // CI / any machine without kiro-cli installed. Installing an exit-0 stub
    // lets `add` proceed. We assert on the command tmux is *told* to run
    // (`pane_start_command`), which aoe builds regardless of the binary, so the
    // stub never needs to behave like real kiro-cli.
    h.install_path_command("kiro-cli");

    let project = h.project_path();
    let mut args = vec![
        "add",
        project.to_str().unwrap(),
        "-t",
        title,
        "--tool",
        "kiro",
        "--launch",
    ];
    args.extend_from_slice(extra);
    let _ = h.run_cli(&args);

    let session = launched_tmux_name(h, title);
    let guard = TmuxSessionGuard(session.clone());
    let cmd = pane_start_command(&session);
    (cmd, guard)
}

#[test]
#[serial]
fn test_kiro_launches_via_chat_subcommand() {
    require_tmux!();

    let mut h = TuiTestHarness::new("kiro_launch_chat");
    let (cmd, _guard) = launch_kiro_and_read_command(&mut h, "KiroChat", &[]);

    assert!(
        cmd.contains("kiro-cli chat"),
        "kiro must launch via `kiro-cli chat`, got: {cmd:?}"
    );
}

#[test]
#[serial]
fn test_kiro_yolo_passes_trust_all_tools_after_chat() {
    require_tmux!();

    let mut h = TuiTestHarness::new("kiro_launch_yolo");
    let (cmd, _guard) = launch_kiro_and_read_command(&mut h, "KiroYolo", &["--yolo"]);

    // The fix: YOLO mode must produce a parseable command. `kiro-cli chat` must
    // appear and `--trust-all-tools` must follow it; bare
    // `kiro-cli --trust-all-tools` is what the CLI rejected.
    let chat = cmd
        .find("kiro-cli chat")
        .unwrap_or_else(|| panic!("`kiro-cli chat` not in launch command: {cmd:?}"));
    let yolo = cmd
        .find("--trust-all-tools")
        .unwrap_or_else(|| panic!("`--trust-all-tools` not in launch command: {cmd:?}"));
    assert!(
        yolo > chat,
        "--trust-all-tools must come after `kiro-cli chat`, got: {cmd:?}"
    );
}

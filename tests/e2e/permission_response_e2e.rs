//! E2E coverage for the sidebar permission-response action: pressing `a`
//! on a session showing a CLI permission prompt sends the agent's mapped
//! keystroke sequence straight to the tmux pane, without attaching.

use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

/// Write a fake "claude" script that prints a static permission-prompt
/// block, then blocks reading exactly one byte from stdin via `dd` (not
/// `read -r`, which would wait for a newline the app never sends; the
/// real Claude Code CLI selects a numbered menu option on a bare digit
/// with no Enter, and `send_key_tokens` deliberately sends no implicit
/// trailing key). Once a byte arrives, it echoes it in a recognizable,
/// greppable form and sleeps so the pane stays alive for the assertion.
///
/// Returns the script's absolute path, for use with `--cmd-override`
/// rather than relying on `$PATH`: the tmux pane that runs the agent
/// launches it through a login shell (so version-manager PATHs like
/// NVM load), and that shell's own rc files can reorder `$PATH` ahead of
/// a `$PATH`-installed stub, letting a real `claude` on the host shadow
/// it. An absolute `--cmd-override` path sidesteps PATH lookup entirely.
fn install_fake_claude_prompt(h: &TuiTestHarness) -> std::path::PathBuf {
    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let claude = dir.join("fake-claude");
    // The pane's tty starts in canonical line-buffered mode, so a bare
    // digit with no trailing Enter (exactly what `send_key_tokens` sends)
    // sits in the kernel tty driver's line buffer forever; `dd` would
    // never see it without a newline. Disable icanon/echo first so a
    // single byte is delivered to `dd` as soon as it arrives, matching
    // how a real full-screen TUI (raw mode) actually reads a keypress.
    let script = "#!/bin/sh\n\
echo 'Do you want to proceed?'\n\
echo '1. Yes'\n\
echo \"2. Yes, and don't ask again\"\n\
echo '3. No'\n\
stty -icanon -echo 2>/dev/null\n\
char=$(dd bs=1 count=1 2>/dev/null)\n\
stty icanon echo 2>/dev/null\n\
echo \"GOT:[$char]\"\n\
sleep 60\n";
    std::fs::write(&claude, script).expect("write fake claude prompt script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");
    }
    claude
}

#[test]
#[serial]
fn test_respond_to_permission_allow_sends_bare_digit() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_allow");
    let fake_claude = install_fake_claude_prompt(&h);

    let project = h.project_path();
    // `--launch` starts the tmux session (spawning the fake claude script)
    // and then tries to attach the CLI process's own terminal to it; that
    // attach fails here because `run_cli` has no tty, but the tmux session
    // is already live by that point, which is all this test needs. Don't
    // assert success; asserting on it would depend on that terminal-attach
    // side effect, not on session creation/launch.
    h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "PermRespTest",
        "--tool",
        "claude",
        "--cmd-override",
        fake_claude.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("PermRespTest");
    // The fake claude's static prompt block should already be visible in
    // the attached-by-default preview / pane.
    h.wait_for("Do you want to proceed?");

    // Open the respond-to-permission dialog for the selected session.
    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");

    // Direct mnemonic 'a' = Allow, mirroring structured_view's a/A/d
    // convention. Submits immediately (no Enter needed).
    h.send_keys("a");

    // The shim should have received exactly the bare digit "1" (claude's
    // PermissionResponse.allow), with nothing else injected before or
    // after it.
    h.wait_for("GOT:[1]");
}

#[test]
#[serial]
fn test_respond_to_permission_deny_sends_bare_digit() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_deny");
    let fake_claude = install_fake_claude_prompt(&h);

    let project = h.project_path();
    // See the allow test above for why the add-launch output isn't asserted.
    h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "PermRespDeny",
        "--tool",
        "claude",
        "--cmd-override",
        fake_claude.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("PermRespDeny");
    h.wait_for("Do you want to proceed?");

    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("d");

    h.wait_for("GOT:[3]");
}

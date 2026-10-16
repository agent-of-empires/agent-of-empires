//! E2E coverage for the sidebar permission-response action: pressing `a`
//! on a session showing a CLI permission prompt sends the agent's mapped
//! keystroke sequence straight to the tmux pane, without attaching.

use serial_test::parallel;

use crate::harness::{require_tmux, wait_until, TuiTestHarness};

/// Write a fake agent script that prints a static permission-prompt block,
/// then blocks reading exactly one byte from stdin via `dd` (not `read -r`,
/// which would wait for a newline the app never sends). Once a byte arrives,
/// it echoes it in a recognizable, greppable form and sleeps so the pane
/// stays alive for the assertion.
///
/// Returns the script's absolute path, for use with `--cmd-override` rather
/// than relying on `$PATH`: the tmux pane that runs the agent launches it
/// through a login shell (so version-manager PATHs like NVM load), and that
/// shell's own rc files can reorder `$PATH` ahead of a `$PATH`-installed stub,
/// letting a real agent on the host shadow it. An absolute `--cmd-override`
/// path sidesteps PATH lookup entirely.
fn install_fake_prompt(h: &TuiTestHarness, tool: &str) -> std::path::PathBuf {
    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let prompt = dir.join(format!("fake-{tool}"));
    // The pane's tty starts in canonical line-buffered mode, so a bare
    // digit with no trailing Enter (exactly what `send_key_tokens` sends)
    // sits in the kernel tty driver's line buffer forever; `dd` would
    // never see it without a newline. Disable icanon/echo first so a
    // single byte is delivered to `dd` as soon as it arrives, matching
    // how a real full-screen TUI (raw mode) actually reads a keypress.
    // The cursor on the first option is load-bearing, not decoration: the
    // permission-prompt detector reads a menu by its highlighted option, so a
    // cursorless list is prose to it and this pane would stop reading Waiting
    // without any assertion here going red.
    let script = "#!/bin/sh\n\
echo 'Do you want to proceed?'\n\
echo '\u{276f} 1. Yes'\n\
echo \"2. Yes, and don't ask again\"\n\
echo '3. No'\n\
stty -icanon -echo 2>/dev/null\n\
char=$(dd bs=1 count=1 2>/dev/null)\n\
stty icanon echo 2>/dev/null\n\
echo \"GOT:[$char]\"\n\
sleep 60\n";
    std::fs::write(&prompt, script).expect("write fake prompt script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prompt, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake prompt script");
    }
    prompt
}

#[test]
#[parallel]
fn test_respond_to_permission_allow_sends_bare_digit() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_allow");
    let fake_claude = install_fake_prompt(&h, "claude");

    let project = h.project_path();
    // `--launch` starts the tmux session (spawning the fake claude script)
    // and skips the interactive terminal-attach step, since `run_cli` has no
    // controlling terminal; the exit status is asserted to cover that.
    let output = h.run_cli(&[
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
    assert!(
        output.status.success(),
        "aoe add --launch should succeed without a controlling terminal: {}",
        String::from_utf8_lossy(&output.stderr)
    );

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

/// OMP's approval chord is distinct from the arrows and Enter used by question selectors.
#[test]
#[parallel]
fn test_respond_to_omp_approval_uses_approval_shortcuts() {
    require_tmux!();

    let mut h = TuiTestHarness::new("omp_perm_resp");
    h.install_path_command("omp");
    let fake_omp = h.home_path().join("fake-omp");
    let script = "#!/bin/sh\n\
for request in 1 2; do\n\
  printf 'Allow tool: bash\\n'\n\
  printf 'Command: printf approval-%s\\n' \"$request\"\n\
  printf 'Ctrl+Enter / Ctrl+Q approve · Esc deny\\n'\n\
  stty -icanon -echo -ixon min 1 time 0 2>/dev/null\n\
  first=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d '[:space:]')\n\
  stty min 0 time 5 2>/dev/null\n\
  rest=$(dd bs=1 count=15 2>/dev/null | od -An -tx1 | tr -d '[:space:]')\n\
  bytes=\"${first}${rest}\"\n\
  stty icanon echo ixon 2>/dev/null\n\
  printf 'GOT[%s]:[%s]\\n' \"$request\" \"$bytes\"\n\
done\n\
sleep 60\n";
    std::fs::write(&fake_omp, script).expect("write fake OMP prompt");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_omp, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake OMP prompt");
    }

    let project = h.project_path();
    let session_id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        "OmpPermRespTest",
        "--tool",
        "omp",
        "--cmd-override",
        fake_omp.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("OmpPermRespTest");
    h.wait_for("Allow tool: bash");

    let capture = wait_until(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(200),
        || {
            let out = h.run_cli(&["session", "capture", &session_id, "--json"]);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let parsed: serde_json::Value =
                serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null);
            if parsed["content"]
                .as_str()
                .is_some_and(|content| content.contains("Allow tool: bash"))
                && parsed["status"] == "waiting"
            {
                Ok(parsed)
            } else {
                Err(format!("OMP approval not Waiting: {stdout}"))
            }
        },
    );
    assert_eq!(capture["status"], "waiting");

    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("a");
    h.wait_for("GOT[1]:[11]");

    h.wait_for("approval-2");
    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("d");
    h.wait_for("GOT[2]:[1b]");
}

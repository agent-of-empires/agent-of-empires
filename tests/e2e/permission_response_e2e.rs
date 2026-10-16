//! E2E coverage for the sidebar permission-response action: pressing `a`
//! on a session showing a CLI permission prompt sends the agent's mapped
//! keystroke sequence straight to the tmux pane, without attaching.

use serial_test::parallel;

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
#[parallel]
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
#[parallel]
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

fn install_fake_integration_plugin(h: &TuiTestHarness) {
    let plugin = h.home_path().join("fake-integration");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("aoe-plugin.toml"),
        r#"
id = "acme.fake-integration"
name = "Fake provider integration"
version = "0.1.0"
api_version = 13
capabilities = ["session.integration"]

[[agent_integrations]]
tool = "claude"
"#,
    )
    .unwrap();
    let output = h.run_cli(&["plugin", "install", plugin.to_str().unwrap(), "--yes"]);
    assert!(
        output.status.success(),
        "install fake integration failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install_fake_integration_agent(h: &TuiTestHarness) -> std::path::PathBuf {
    let bin = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let agent = bin.join("fake-claude-integration");
    let script = r#"#!/bin/sh
set -eu
umask 077
dir="/tmp/aoe-hooks-$(id -u)/$AOE_INSTANCE_ID"
mkdir -p "$dir"
printf '%s\n' "$AOE_INSTANCE_ID" > "$HOME/instance-id"
write_report() {
  tmp="$dir/.integration.$$.tmp"
  cat > "$tmp"
  mv "$tmp" "$dir/integration-acme.fake-integration.json"
}
refresh_report() {
  while :; do
    sleep 1
    touch "$dir/integration-acme.fake-integration.json" 2>/dev/null || :
  done
}
publish_approval() {
  write_report <<EOF
{"schema_version":1,"instance_id":"$AOE_INSTANCE_ID","provider_session_id":"fake-provider-session","writer_pid":$$,"revision":1,"status":"waiting","reply_keys":[{"kind":"named","value":"F12"}],"prompt":{"kind":"approval","id":"approval-one","title":"Fake approval","allow_always":false}}
EOF
}
read_reply() {
  marker="$1"
  key=$(dd bs=5 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
  printf 'GOT:%s:F12:%s\n' "$marker" "$key"
  reply=$(cat "$dir/integration-acme.fake-integration.reply.json")
}
announce_reply() {
  case "$reply" in
    *'"choice":"allow"'*) printf 'PROVIDER_APPROVED\n' ;;
    *'"selected_options":["SQLite"]'*) printf 'PROVIDER_ANSWER_SQLITE\n' ;;
    *) printf 'INVALID_PROVIDER_REPLY\n' ;;
  esac
}
stty -icanon -echo 2>/dev/null
publish_approval
refresh_report &
refresh_pid=$!
trap 'kill "$refresh_pid" 2>/dev/null || true' EXIT
printf 'Fake approval\n'
read_reply approval
write_report <<EOF
{"schema_version":1,"instance_id":"$AOE_INSTANCE_ID","provider_session_id":"fake-provider-session","writer_pid":$$,"revision":2,"status":"waiting","reply_keys":[{"kind":"named","value":"F12"}],"prompt":{"kind":"question","id":"question-one","title":"Choose database","questions":[{"id":"database","question":"Database?","options":[{"label":"SQLite"},{"label":"Postgres"}],"multi":false,"recommended":0}]}}
EOF
announce_reply
printf 'Provider question ready\n'
read_reply question
announce_reply
sleep 60
"#;
    std::fs::write(&agent, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    agent
}

#[test]
#[parallel]
fn provider_report_drives_approval_and_question_replies_over_tmux() {
    require_tmux!();

    let mut h = TuiTestHarness::new("provider_reply");
    install_fake_integration_plugin(&h);
    let fake_agent = install_fake_integration_agent(&h);

    let project = h.project_path();
    h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "ProviderReply",
        "--tool",
        "claude",
        "--cmd-override",
        fake_agent.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for("ProviderReply");
    h.wait_for("Fake approval");
    let reply_path = fake_provider_reply_path(&h);
    let sidecar_cleanup = FakeProviderSidecarCleanup(reply_path.parent().unwrap().to_path_buf());
    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("a");
    h.wait_for("GOT:approval:F12:1b5b32347e");
    h.wait_for("PROVIDER_APPROVED");
    h.wait_for("Provider question ready");

    h.send_keys("a");
    h.wait_for("Database?");
    h.wait_for("Choose database");
    h.send_keys("Enter");
    h.send_keys("C-s");
    h.wait_for("GOT:question:F12:1b5b32347e");
    h.wait_for("PROVIDER_ANSWER_SQLITE");
    drop(h);
    drop(sidecar_cleanup);
}

fn install_fake_stale_prompt_agent(h: &TuiTestHarness) -> std::path::PathBuf {
    let bin = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let agent = bin.join("fake-stale-claude");
    let script = r#"#!/bin/sh
set -eu
umask 077
dir="/tmp/aoe-hooks-$(id -u)/$AOE_INSTANCE_ID"
mkdir -p "$dir"
printf '%s\n' "$AOE_INSTANCE_ID" > "$HOME/instance-id"
tmp="$dir/.integration.$$.tmp"
cat > "$tmp" <<EOF
{"schema_version":1,"instance_id":"$AOE_INSTANCE_ID","provider_session_id":"fake-provider-session","writer_pid":$$,"revision":1,"status":"waiting","reply_keys":[{"kind":"named","value":"F12"}],"prompt":{"kind":"approval","id":"approval-one","title":"Fake approval","allow_always":false}}
EOF
mv "$tmp" "$dir/integration-acme.fake-integration.json"
printf 'Fake approval\n'
refresh_report() {
  while :; do
    sleep 1
    touch "$dir/integration-acme.fake-integration.json" 2>/dev/null || :
  done
}
refresh_report &
refresh_pid=$!
trap 'kill "$refresh_pid" 2>/dev/null || true' EXIT
stty -icanon -echo 2>/dev/null
key=$(dd bs=5 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
printf 'GOT:F12:%s\n' "$key"
sleep 60
"#;
    std::fs::write(&agent, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    agent
}

fn fake_provider_reply_path(h: &TuiTestHarness) -> std::path::PathBuf {
    let instance_id = std::fs::read_to_string(h.home_path().join("instance-id"))
        .unwrap()
        .trim()
        .to_owned();
    std::path::PathBuf::from(format!(
        "/tmp/aoe-hooks-{}/{}/integration-acme.fake-integration.reply.json",
        nix::unistd::geteuid().as_raw(),
        instance_id
    ))
}

struct FakeProviderSidecarCleanup(std::path::PathBuf);

impl Drop for FakeProviderSidecarCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn replace_fake_provider_prompt(h: &TuiTestHarness) {
    use std::os::unix::fs::OpenOptionsExt;
    let reply_path = fake_provider_reply_path(h);
    let dir = reply_path.parent().unwrap();
    let report_path = dir.join("integration-acme.fake-integration.json");
    let mut report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    report["revision"] = serde_json::json!(2);
    report["prompt"]["id"] = serde_json::json!("approval-two");
    report["prompt"]["title"] = serde_json::json!("Changed approval");
    let temporary = dir.join(".integration-test.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .unwrap();
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&report).unwrap())
        .unwrap();
    file.sync_all().unwrap();
    std::fs::rename(temporary, &report_path).unwrap();
}

#[test]
#[parallel]
fn provider_reply_cancel_and_stale_prompt_never_send_tmux_keys() {
    require_tmux!();

    let mut h = TuiTestHarness::new("provider_stale_reply");
    install_fake_integration_plugin(&h);
    let fake_agent = install_fake_stale_prompt_agent(&h);
    let project = h.project_path();
    h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "ProviderStaleReply",
        "--tool",
        "claude",
        "--cmd-override",
        fake_agent.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for("ProviderStaleReply");
    h.wait_for("Fake approval");

    let reply_path = fake_provider_reply_path(&h);
    let sidecar_cleanup = FakeProviderSidecarCleanup(reply_path.parent().unwrap().to_path_buf());
    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("Escape");
    h.wait_for_absent(
        "Respond to Permission Prompt",
        std::time::Duration::from_secs(3),
    );
    assert!(
        !reply_path.exists(),
        "cancel must not write a provider reply"
    );

    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    replace_fake_provider_prompt(&h);
    h.send_keys("a");
    h.wait_for("Respond Failed");
    h.wait_for("prompt changed");
    assert!(
        !reply_path.exists(),
        "stale prompt must be rejected before writing a reply"
    );
    h.assert_screen_not_contains("GOT:F12");
    drop(h);
    drop(sidecar_cleanup);
}

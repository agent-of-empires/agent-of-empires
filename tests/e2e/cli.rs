use serial_test::parallel;
use std::path::Path;
use std::process::Command;

use crate::harness::{require_tmux, TuiTestHarness};

/// Helper to read a session field from the sessions.json in the harness's isolated home.
fn read_sessions_json(h: &TuiTestHarness) -> serde_json::Value {
    let sessions_path =
        crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let content = std::fs::read_to_string(&sessions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", sessions_path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

#[test]
#[parallel]
fn test_cli_add_and_list() {
    let h = TuiTestHarness::new("cli_add_list");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "E2E Test Session"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let list_output = h.run_cli(&["list"]);
    assert!(
        list_output.status.success(),
        "aoe list failed: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );

    let stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        stdout.contains("E2E Test Session"),
        "list output should contain session title.\nOutput:\n{}",
        stdout
    );
}

/// #3224: repeating `aoe add` for the same title and path must report a
/// conflict instead of exiting successfully while silently retaining the
/// first session's command override.
#[test]
#[parallel]
fn test_cli_add_duplicate_errors_and_preserves_original_command() {
    let h = TuiTestHarness::new("cli_add_duplicate");
    let project = h.project_path();
    let project = project.to_str().unwrap();

    let first = h.run_cli(&[
        "add",
        project,
        "--title",
        "Duplicate Session",
        "--cmd-override",
        "echo OLD",
    ]);
    assert!(
        first.status.success(),
        "initial aoe add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let duplicate = h.run_cli(&[
        "add",
        project,
        "--title",
        "Duplicate Session",
        "--cmd-override",
        "echo NEW",
    ]);
    assert!(
        !duplicate.status.success(),
        "duplicate aoe add must return a non-zero status"
    );
    assert_eq!(
        duplicate.status.code(),
        Some(1),
        "duplicate aoe add should use the CLI's ordinary error exit status"
    );
    let stderr = String::from_utf8_lossy(&duplicate.stderr);
    assert!(
        stderr.contains("Session already exists with same title and path")
            && stderr.contains("different title"),
        "duplicate error should identify the collision and offer remediation.\nstderr: {stderr}"
    );

    let sessions = read_sessions_json(&h);
    let sessions = sessions.as_array().expect("sessions array");
    assert_eq!(
        sessions.len(),
        1,
        "duplicate add must not create a second row"
    );
    assert_eq!(
        sessions[0]["command"], "echo OLD",
        "duplicate add must preserve the original command override"
    );
}

/// Regression test for #848: `aoe add` "Next steps" hint should reference
/// the actual binary name (`aoe`), not the long project name.
#[test]
#[parallel]
fn test_cli_add_next_steps_uses_aoe_binary_name() {
    let h = TuiTestHarness::new("cli_add_next_steps_name");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "NextStepsName"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let stdout = String::from_utf8_lossy(&add_output.stdout);
    assert!(
        stdout.contains("aoe session start NextStepsName"),
        "expected 'aoe session start' hint in next steps.\nOutput:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("agent-of-empires session start"),
        "next steps should not reference the old 'agent-of-empires' name.\nOutput:\n{}",
        stdout
    );
}

/// #1996: `aoe mcp list --json` shows the merged effective MCP set with
/// per-server provenance and redacts every secret value (env/header values
/// reduced to names). Native (claude) + global mcp.json are merged; global wins
/// a name collision. The native config also carries a secret that must never
/// reach stdout.
#[test]
#[parallel]
fn test_cli_mcp_list_provenance_and_redaction() {
    let h = TuiTestHarness::new("cli_mcp_list");
    let home = h.home_path();

    // Native (claude) layer: defines "shared" and "native-only", plus a secret.
    std::fs::write(
        home.join(".claude.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-native" },
            "native-only": { "command": "n", "env": { "TOKEN": "SUPER_SECRET_DO_NOT_LEAK" } }
        } }"#,
    )
    .expect("write .claude.json");

    // Global layer: overrides "shared" and adds "global-only".
    let app_dir = crate::harness::app_dir_in(home);
    std::fs::create_dir_all(&app_dir).expect("create app dir");
    std::fs::write(
        app_dir.join("mcp.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-global" },
            "global-only": { "command": "g" }
        } }"#,
    )
    .expect("write mcp.json");

    let out = h.run_cli(&["mcp", "list", "--agent", "claude", "--json"]);
    assert!(
        out.status.success(),
        "aoe mcp list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // No secret value may ever reach stdout.
    assert!(
        !stdout.contains("SUPER_SECRET_DO_NOT_LEAK"),
        "secret env value leaked to CLI output:\n{stdout}"
    );

    let val: serde_json::Value = serde_json::from_str(&stdout).expect("output is JSON");
    let effective = val["effective"].as_array().expect("effective array");
    assert_eq!(effective.len(), 3, "native + global union, got {stdout}");

    let shared = effective
        .iter()
        .find(|s| s["name"] == "shared")
        .expect("shared present");
    assert_eq!(
        shared["command"], "from-global",
        "global must win the name collision"
    );
    assert_eq!(shared["provenance"], "global");

    let native_only = effective
        .iter()
        .find(|s| s["name"] == "native-only")
        .expect("native-only present");
    assert_eq!(native_only["provenance"], "agent-native:claude");
    // The secret env var is reported by NAME only.
    assert_eq!(native_only["envNames"], serde_json::json!(["TOKEN"]));
}

/// #3050: the CLI can discover, adopt, view, edit, and remove a skill while
/// keeping the external source untouched.
#[test]
#[parallel]
fn test_cli_skill_management_flow() {
    let h = TuiTestHarness::new("cli_skill_management");
    let source = h.home_path().join(".claude/skills/review");
    std::fs::create_dir_all(&source).unwrap();
    let original = "---\nname: review\ndescription: Review code\n---\n\nOriginal body\n";
    std::fs::write(source.join("SKILL.md"), original).unwrap();

    let list = h.run_cli(&["skill", "list", "--json"]);
    assert!(
        list.status.success(),
        "skill list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(listed["skills"].as_array().unwrap().iter().any(|skill| {
        skill["directory"] == "review"
            && skill["provenance"]["root"] == "claude-user"
            && skill["provenance"]["kind"] == "external"
    }));

    let adopt = h.run_cli(&["skill", "adopt", "claude-user", "review"]);
    assert!(
        adopt.status.success(),
        "skill adopt failed: {}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
        original
    );

    let edited = "---\nname: review\ndescription: Updated\n---\n\nEdited body\n";
    let edit = h.run_cli_with_stdin(&["skill", "edit", "review", "--file", "-"], edited);
    assert!(
        edit.status.success(),
        "skill edit failed: {}",
        String::from_utf8_lossy(&edit.stderr)
    );
    let view = h.run_cli(&["skill", "view", "review"]);
    assert!(view.status.success());
    assert_eq!(String::from_utf8(view.stdout).unwrap(), edited);

    // Sharing puts the managed skill in every agent's own skills dir, and the
    // copy is listed as its managed original rather than as a second external
    // skill of the same name.
    let sync = h.run_cli(&["skill", "sync", "--json"]);
    assert!(
        sync.status.success(),
        "skill sync failed: {}",
        String::from_utf8_lossy(&sync.stderr)
    );
    for root in ["gemini/skills", "agents/skills", "config/opencode/skills"] {
        let landed = h.home_path().join(format!(".{root}/review/SKILL.md"));
        assert!(landed.is_file(), "{} missing after sync", landed.display());
    }
    let listed: serde_json::Value =
        serde_json::from_slice(&h.run_cli(&["skill", "list", "--json"]).stdout).unwrap();
    let sources: Vec<&str> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["directory"] == "review")
        .map(|s| s["provenance"]["root"].as_str().unwrap_or("aoe-managed"))
        .collect();
    // The copies AoE just wrote are not listed again: they are the managed
    // skill, not three more external ones. The user's own claude-user package
    // is untouched, so it is still its own entry, and sync reported it as a
    // conflict rather than replacing it.
    assert_eq!(
        sources,
        vec!["aoe-managed", "claude-user"],
        "propagated copies must not be double-counted"
    );

    // The user's own claude-user package is a conflict, so an ordinary sync
    // leaves it alone; naming it takes it over, and it stays current after.
    let conflicted = h.run_cli(&["skill", "sync", "--root", "claude-user", "--json"]);
    let outcomes: serde_json::Value = serde_json::from_slice(&conflicted.stdout).unwrap();
    assert_eq!(outcomes[0]["status"], "conflict", "{outcomes}");
    assert_eq!(
        std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
        original
    );

    let replaced = h.run_cli(&[
        "skill",
        "sync",
        "--root",
        "claude-user",
        "--replace",
        "review",
        "--json",
    ]);
    let outcomes: serde_json::Value = serde_json::from_slice(&replaced.stdout).unwrap();
    assert_eq!(outcomes[0]["status"], "updated", "{outcomes}");
    assert_eq!(
        std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
        edited,
        "replace should install the managed content"
    );

    // Deleting the managed source and re-syncing withdraws the copies it made.
    let remove = h.run_cli(&["skill", "remove", "review"]);
    assert!(remove.status.success());
    assert!(!crate::harness::app_dir_in(h.home_path())
        .join("skills/review")
        .exists());
    assert!(h.run_cli(&["skill", "sync"]).status.success());
    assert!(!h.home_path().join(".gemini/skills/review").exists());
    // Including the claude-user copy: taking it over above made it AoE-owned,
    // so withdrawing is symmetric with having deployed it. The never-overwrite
    // rule is what the conflict assertion earlier covers.
    assert!(
        !source.exists(),
        "a replaced copy is AoE-owned, so it withdraws with its source"
    );
}

/// Native Codex entries with `enabled = false` remain known to drift tracking
/// but do not appear in the effective set printed by `aoe mcp list`.
#[test]
#[parallel]
fn test_cli_mcp_list_honors_codex_enabled_flag() {
    let h = TuiTestHarness::new("cli_mcp_codex_enabled");
    let home = h.home_path();
    let codex_dir = home.join(".codex");
    std::fs::create_dir_all(&codex_dir).expect("create .codex dir");
    std::fs::write(
        codex_dir.join("config.toml"),
        r#"
[mcp_servers.omitted]
command = "omitted"

[mcp_servers.explicit_true]
command = "true"
enabled = true

[mcp_servers.explicit_false]
command = "false"
enabled = false
"#,
    )
    .expect("write Codex config");

    let out = h.run_cli(&["mcp", "list", "--agent", "codex", "--json"]);
    assert!(
        out.status.success(),
        "aoe mcp list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).expect("output is JSON");
    let effective = val["effective"].as_array().expect("effective array");
    let names = effective
        .iter()
        .map(|server| server["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["explicit_true", "omitted"]);
    assert_eq!(val["keptOnRemoval"], serde_json::json!([]));
    assert_eq!(val["conflicts"], serde_json::json!([]));
    assert_eq!(val["driftPaused"], false);
}

/// #1909: `aoe add --interactive` must fail loudly when stdin is not a
/// terminal instead of hanging on the name prompt. `run_cli` runs the
/// binary as a plain subprocess with no controlling TTY, which is the
/// non-interactive case the guard protects.
#[test]
#[parallel]
fn test_cli_add_interactive_requires_tty() {
    let h = TuiTestHarness::new("cli_add_interactive_no_tty");
    let project = h.project_path();

    let output = h.run_cli(&["add", project.to_str().unwrap(), "-i"]);
    assert!(
        !output.status.success(),
        "aoe add -i without a TTY should fail, not hang or succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires a terminal"),
        "expected the --interactive TTY guard message.\nstderr: {}",
        stderr
    );

    // The guard runs before any persistence, so no session row is written.
    let sessions_path =
        crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    assert!(
        !sessions_path.exists(),
        "the TTY guard must bail before writing sessions.json"
    );
}

/// #1909: `aoe add --interactive` should prompt for a session name like
/// the TUI `n` flow. Driven through a tmux pane so stdin is a real
/// terminal; the typed name must become the session title.
#[test]
#[parallel]
fn test_cli_add_interactive_prompts_for_name() {
    require_tmux!();

    let mut h = TuiTestHarness::new("cli_add_interactive_prompt");
    let project = h.project_path();
    let project_arg = project.to_str().unwrap().to_string();

    h.spawn(&["add", &project_arg, "-i"]);
    h.wait_for("Session name [");
    h.type_text("InteractivePrompted");
    h.send_keys("Enter");

    // The add command exits as soon as it persists, tearing down the tmux
    // pane, so the screen goes blank. Poll the on-disk session store
    // instead of waiting on screen output.
    let sessions_path =
        crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let landed = loop {
        let found = std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|j| {
                j.as_array().is_some_and(|sessions| {
                    sessions
                        .iter()
                        .any(|s| s["title"].as_str() == Some("InteractivePrompted"))
                })
            })
            .unwrap_or(false);
        if found {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    assert!(
        landed,
        "interactive prompt should persist a session titled InteractivePrompted"
    );
}

#[test]
#[parallel]
fn test_cli_add_invalid_path() {
    let h = TuiTestHarness::new("cli_add_invalid");

    let output = h.run_cli(&["add", "/nonexistent/path/that/does/not/exist"]);
    assert!(
        !output.status.success(),
        "aoe add should fail for nonexistent path"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not")
            || combined.contains("exist")
            || combined.contains("error")
            || combined.contains("Error")
            || combined.contains("invalid")
            || combined.contains("No such"),
        "expected error message about invalid path.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
}

#[test]
#[parallel]
fn test_cli_add_respects_config_extra_args() {
    let h = TuiTestHarness::new("cli_add_config_extra_args");
    let project = h.project_path();

    // Write config with agent_extra_args for claude
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
default_tool = "claude"
agent_extra_args = {{ claude = "--verbose --debug" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "ConfigExtraArgs"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["extra_args"].as_str().unwrap_or(""),
        "--verbose --debug",
        "extra_args should be populated from config"
    );
}

#[test]
#[parallel]
fn test_cli_add_respects_config_command_override() {
    let h = TuiTestHarness::new("cli_add_config_cmd_override");
    let project = h.project_path();

    // Write config with agent_command_override for claude
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
default_tool = "claude"
agent_command_override = {{ claude = "my-custom-claude" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "ConfigCmdOverride"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["command"].as_str().unwrap_or(""),
        "my-custom-claude",
        "command should be populated from config agent_command_override"
    );
}

#[test]
#[parallel]
fn test_cli_add_cmd_respects_command_override_for_availability() {
    // `qwen` is a built-in agent whose binary is not installed in CI. The
    // override remaps it to a wrapper that we shim on PATH. Pre-fix, the
    // `--cmd` availability check ran `which qwen` and bailed because the
    // bare binary was absent; post-fix it verifies the override binary
    // (`qwen-plannotator`) that will actually launch. See #1910.
    let mut h = TuiTestHarness::new("cli_add_cmd_override_avail");
    h.install_path_command("qwen-plannotator");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
agent_command_override = {{ qwen = "qwen-plannotator" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "QwenOverride",
        "--cmd",
        "qwen",
    ]);
    assert!(
        add_output.status.success(),
        "aoe add --cmd qwen should succeed when the override binary is on PATH: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["tool"].as_str().unwrap_or(""),
        "qwen",
        "tool should resolve to the built-in qwen"
    );
    assert_eq!(
        session["command"].as_str().unwrap_or(""),
        "qwen-plannotator",
        "command should resolve through session.agent_command_override"
    );
}

#[test]
#[parallel]
fn test_cli_add_cli_flags_override_config() {
    let h = TuiTestHarness::new("cli_add_flags_override");
    let project = h.project_path();

    // Write config with agent_extra_args for claude
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
default_tool = "claude"
agent_extra_args = {{ claude = "--from-config" }}
agent_command_override = {{ claude = "config-claude" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    // CLI flags should take priority over config
    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "FlagsOverride",
        "--extra-args",
        "from-cli-extra",
        "--cmd-override",
        "cli-claude",
    ]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["extra_args"].as_str().unwrap_or(""),
        "from-cli-extra",
        "CLI --extra-args should override config"
    );
    assert_eq!(
        session["command"].as_str().unwrap_or(""),
        "cli-claude",
        "CLI --cmd-override should override config"
    );
}

#[test]
#[parallel]
fn test_cli_add_respects_default_tool() {
    let h = TuiTestHarness::new("cli_add_default_tool");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
default_tool = "opencode"
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "DefaultTool"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["tool"].as_str().unwrap_or(""),
        "opencode",
        "tool should be 'opencode' from default_tool config"
    );
    assert_eq!(
        session["command"].as_str().unwrap_or(""),
        "opencode",
        "command should be 'opencode' via set_default_command"
    );
}

#[test]
#[parallel]
fn test_cli_add_cmd_overrides_default_tool() {
    let h = TuiTestHarness::new("cli_add_cmd_overrides");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
default_tool = "opencode"
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "CmdOverride",
        "--cmd",
        "claude",
    ]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["tool"].as_str().unwrap_or(""),
        "claude",
        "explicit --cmd should override default_tool config"
    );
}

#[test]
#[parallel]
fn test_cli_add_respects_yolo_mode_default() {
    let h = TuiTestHarness::new("cli_add_yolo_default");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
yolo_mode_default = true
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "YoloDefault"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["yolo_mode"].as_bool(),
        Some(true),
        "yolo_mode should be true from yolo_mode_default config"
    );
}

#[test]
#[parallel]
fn test_cli_add_yolo_flag_without_config() {
    let h = TuiTestHarness::new("cli_add_yolo_flag");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "YoloFlag", "--yolo"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(
        session["yolo_mode"].as_bool(),
        Some(true),
        "--yolo flag should set yolo_mode to true"
    );
}

#[test]
#[parallel]
fn test_cli_add_default_tool_no_config() {
    let h = TuiTestHarness::new("cli_add_no_config");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "NoConfig"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    // Harness prepends a fake `claude` binary to PATH, so no-config tool
    // selection should deterministically choose `claude`.
    let expected = "claude";
    assert_eq!(
        session["tool"].as_str().unwrap_or(""),
        expected,
        "tool should default to first available tool ('{}') when no default_tool config",
        expected
    );
}

#[test]
#[parallel]
fn cli_add_custom_agent_persists_configured_command_extra_args_and_detect_as() {
    let h = TuiTestHarness::new("cli_add_custom_agent_success");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "bash -lc true" }}
agent_detect_as = {{ custom = "claude" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "custom",
        "-t",
        "CustomTool",
        "--extra-args",
        "--flag value",
    ]);
    assert!(
        add_output.status.success(),
        "aoe add --tool custom failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(session["tool"].as_str().unwrap_or(""), "custom");
    assert_eq!(session["command"].as_str().unwrap_or(""), "bash -lc true");
    assert_eq!(session["extra_args"].as_str().unwrap_or(""), "--flag value");
    assert_eq!(session["detect_as"].as_str().unwrap_or(""), "claude");
}

#[test]
#[parallel]
fn cli_add_custom_agent_allows_missing_detect_as_mapping() {
    let h = TuiTestHarness::new("cli_add_custom_agent_no_detect_as");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "bash -lc true" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "--tool", "custom"]);
    assert!(
        add_output.status.success(),
        "aoe add --tool custom should not require agent_detect_as:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(session["tool"].as_str().unwrap_or(""), "custom");
    assert_eq!(session["command"].as_str().unwrap_or(""), "bash -lc true");
    assert_eq!(session["detect_as"].as_str().unwrap_or(""), "");
}

#[test]
#[parallel]
fn cli_add_custom_agent_unknown_tool_fails_safely_without_persistence() {
    let h = TuiTestHarness::new("cli_add_custom_agent_unknown");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "secret-custom-command-for-leak-check" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let output = h.run_cli(&["add", project.to_str().unwrap(), "--tool", "missing"]);
    assert!(!output.status.success(), "unknown tool should fail");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("custom") || combined.contains("claude"),
        "error should list safe built-in or custom names. Output:\n{}",
        combined
    );
    assert!(
        !combined.contains("secret-custom-command-for-leak-check"),
        "error must not leak configured command string. Output:\n{}",
        combined
    );

    let sessions_path = config_dir.join("profiles/default/sessions.json");
    assert!(
        !sessions_path.exists(),
        "unknown tool must fail before writing sessions.json"
    );
}

#[test]
#[parallel]
fn cli_add_custom_agent_rejects_custom_cmd_override() {
    let h = TuiTestHarness::new("cli_add_custom_agent_cmd_override");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "bash -lc true" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "custom",
        "--cmd-override",
        "other",
    ]);
    assert!(
        !output.status.success(),
        "custom --tool should reject --cmd-override"
    );
}

#[test]
#[parallel]
fn cli_add_custom_agent_rejects_empty_configured_command() {
    let h = TuiTestHarness::new("cli_add_custom_agent_empty_command");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let output = h.run_cli(&["add", project.to_str().unwrap(), "--tool", "custom"]);
    assert!(
        !output.status.success(),
        "empty custom-agent command should fail"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("empty") && combined.contains("custom"),
        "error should explain empty custom-agent command. Output:\n{}",
        combined
    );
}

#[test]
#[parallel]
fn cli_add_custom_agent_rejects_invalid_detect_as_target() {
    let h = TuiTestHarness::new("cli_add_custom_agent_bad_detect_as");
    let project = h.project_path();

    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
custom_agents = {{ custom = "bash -lc true" }}
agent_detect_as = {{ custom = "not-a-built-in" }}
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config.toml");

    let output = h.run_cli(&["add", project.to_str().unwrap(), "--tool", "custom"]);
    assert!(
        !output.status.success(),
        "invalid detect_as target should fail"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("agent_detect_as") && combined.contains("not-a-built-in"),
        "error should explain invalid detect_as mapping. Output:\n{}",
        combined
    );
}

#[test]
#[parallel]
fn cli_add_custom_agent_allows_builtin_cmd_override() {
    let h = TuiTestHarness::new("cli_add_builtin_tool_cmd_override");
    let project = h.project_path();

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "claude",
        "--cmd-override",
        "custom-claude",
        "-t",
        "BuiltInOverride",
    ]);
    assert!(
        output.status.success(),
        "built-in --tool should allow --cmd-override:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session = &sessions[0];
    assert_eq!(session["tool"].as_str().unwrap_or(""), "claude");
    assert_eq!(session["command"].as_str().unwrap_or(""), "custom-claude");
}

#[test]
#[parallel]
fn cli_add_custom_agent_tool_conflicts_with_cmd() {
    let h = TuiTestHarness::new("cli_add_tool_cmd_conflict");
    let project = h.project_path();

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "custom",
        "--cmd",
        "claude",
    ]);
    assert!(!output.status.success(), "--tool and --cmd should conflict");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("--tool") || combined.contains("--cmd"),
        "conflict error should mention the conflicting flags. Output:\n{}",
        combined
    );
}

/// `aoe session capture` should return pane content or empty output for a stopped session.
#[test]
#[parallel]
fn test_cli_session_capture_stopped() {
    let h = TuiTestHarness::new("cli_capture_stopped");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "CaptureTest"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"].as_str().expect("session should have id");

    // Capture a session that is not running -- should succeed with empty content
    let capture_output = h.run_cli(&["session", "capture", session_id, "--json"]);
    assert!(
        capture_output.status.success(),
        "aoe session capture failed: {}",
        String::from_utf8_lossy(&capture_output.stderr)
    );

    let stdout = String::from_utf8_lossy(&capture_output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("should be valid JSON");
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["content"], "");
    assert_eq!(json["title"], "CaptureTest");
}

/// `aoe session capture` plain text mode should output raw content.
#[test]
#[parallel]
fn test_cli_session_capture_plain() {
    let h = TuiTestHarness::new("cli_capture_plain");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "CapturePlain"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"].as_str().expect("session should have id");

    // Plain text capture of stopped session -- empty output, no error
    let capture_output = h.run_cli(&["session", "capture", session_id]);
    assert!(
        capture_output.status.success(),
        "aoe session capture (plain) failed: {}",
        String::from_utf8_lossy(&capture_output.stderr)
    );
}

/// #3625: `aoe session capture` ran the agent's manifest directly, so a
/// profile's own `[[agents.<name>.status_rules]]` never got a say and the CLI
/// could report a different status than the dashboard for the same pane. The
/// tool has to be manifest-backed for the regression to bite, since that is
/// the branch that skipped the rules.
#[test]
#[parallel]
fn test_cli_session_capture_honors_configured_status_rules() {
    require_tmux!();

    let mut h = TuiTestHarness::new("cli_capture_rules");
    h.install_path_command("opencode");

    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let agent = dir.join("fake-rules-agent");
    // The pane text the configured rule matches. No opencode manifest rule
    // reads it as Waiting, so a Waiting verdict can only come from the rule.
    std::fs::write(&agent, "#!/bin/sh\necho 'deploy to prod?'\nsleep 60\n")
        .expect("write fake agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake agent script");
    }

    let config_path = crate::harness::app_dir_in(h.home_path()).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!(
            "{seeded}\n\n[[agents.opencode.status_rules]]\n\
             status = \"waiting\"\ncontains = \"deploy to prod?\"\n"
        ),
    )
    .expect("write status rules config");

    let project = h.project_path();
    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "CaptureRules",
        "--tool",
        "opencode",
        "--cmd-override",
        agent.to_str().unwrap(),
    ]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session should have id")
        .to_string();

    let start_output = h.run_cli(&["session", "start", &session_id]);
    assert!(
        start_output.status.success(),
        "aoe session start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // The rule can only match once the pane has painted, so poll the CLI
    // itself rather than sleeping on a guess.
    let mut json = serde_json::Value::Null;
    for _ in 0..50 {
        let out = h.run_cli(&["session", "capture", &session_id, "--json"]);
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
                json = parsed;
                if json["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("deploy to prod?"))
                {
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    assert!(
        json["content"]
            .as_str()
            .is_some_and(|c| c.contains("deploy to prod?")),
        "fake agent never painted into the pane; capture was: {json}"
    );
    assert_eq!(
        json["status"], "waiting",
        "a configured status rule must decide what `aoe session capture` reports (#3625)"
    );
}

/// Renaming a session via CLI should rename the tmux session, not kill it.
/// Regression test for https://github.com/agent-of-empires/agent-of-empires/issues/431
#[test]
#[parallel]
fn test_cli_rename_preserves_tmux_session() {
    require_tmux!();

    let h = TuiTestHarness::new("cli_rename_tmux");
    let sock = h.home_path().join("tmux.sock");
    let project = h.project_path();

    // 1. Add a session
    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "OldName"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    // 2. Read the session ID from storage
    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"].as_str().expect("session should have id");
    let truncated_id = &session_id[..8.min(session_id.len())];

    // 3. Compute the tmux session name that aoe would use
    let old_tmux_name = format!(
        "{}OldName_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );

    // Create a real tmux session with that name (simulates a running session)
    let create = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args([
            "new-session",
            "-d",
            "-s",
            &old_tmux_name,
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "60",
        ])
        .output()
        .expect("tmux new-session");
    assert!(
        create.status.success(),
        "failed to create tmux session: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // 4. Rename the session via CLI
    let rename_output = h.run_cli(&["session", "rename", session_id, "-t", "NewName"]);
    assert!(
        rename_output.status.success(),
        "aoe session rename failed: {}",
        String::from_utf8_lossy(&rename_output.stderr)
    );

    // 5. The old tmux session name should be gone
    let old_exists = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["has-session", "-t", &old_tmux_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        !old_exists,
        "Old tmux session '{}' should no longer exist after rename",
        old_tmux_name
    );

    // 6. The new tmux session name should exist
    let new_tmux_name = format!(
        "{}NewName_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );
    let new_exists = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["has-session", "-t", &new_tmux_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        new_exists,
        "New tmux session '{}' should exist after rename",
        new_tmux_name
    );

    // Cleanup
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["kill-session", "-t", &new_tmux_name])
        .output();
}

/// Removing a session via CLI must kill its agent tmux session. Locks the
/// invariant "session removed implies tmux gone" that the audit identified
/// as untested on every removal path.
#[test]
#[parallel]
fn test_cli_rm_kills_agent_tmux_session() {
    require_tmux!();

    let h = TuiTestHarness::new("cli_rm_tmux");
    let sock = h.home_path().join("tmux.sock");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "RmTarget"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"].as_str().expect("session should have id");
    let truncated_id = &session_id[..8.min(session_id.len())];

    let tmux_name = format!(
        "{}RmTarget_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );

    let create = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "60",
        ])
        .output()
        .expect("tmux new-session");
    assert!(
        create.status.success(),
        "failed to create tmux session: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    let rm_output = h.run_cli(&["rm", session_id, "--force"]);
    assert!(
        rm_output.status.success(),
        "aoe rm failed: {}",
        String::from_utf8_lossy(&rm_output.stderr)
    );

    let still_alive = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["has-session", "-t", &tmux_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        !still_alive,
        "tmux session '{}' should be gone after aoe rm",
        tmux_name
    );

    // Cleanup
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["kill-session", "-t", &tmux_name])
        .output();
}

/// Initialize a bare-minimum git repo at the given path so worktree operations work.
fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    for args in [
        &["init", "-q"][..],
        &["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("run isolated git fixture command");
        assert!(
            output.status.success(),
            "git fixture command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Regression test for #591: repo on_create hooks should execute for multi-repo
/// workspace sessions created via `aoe add --repo`.
#[test]
#[parallel]
fn test_cli_add_workspace_repo_hooks_execute() {
    let h = TuiTestHarness::new("cli_workspace_hooks");

    let project_a = h.home_path().join("project-a");
    let project_b = h.home_path().join("project-b");
    init_git_repo(&project_a);
    init_git_repo(&project_b);

    // Set up repo-level hooks in project-a.
    let hook_marker = h.home_path().join("hook-ran.marker");
    let aoe_config_dir = project_a.join(".agent-of-empires");
    std::fs::create_dir_all(&aoe_config_dir).expect("create .agent-of-empires dir");
    let config = format!(
        "[hooks]\non_create = [\"touch {}\"]\n",
        hook_marker.display()
    );
    std::fs::write(aoe_config_dir.join("config.toml"), &config).expect("write repo config");

    let add_output = h.run_cli(&[
        "add",
        project_a.to_str().unwrap(),
        "--repo",
        project_b.to_str().unwrap(),
        "-w",
        "feat/hook-test",
        "-b",
        "-t",
        "HookTest",
        "--trust-hooks",
    ]);
    let stdout = String::from_utf8_lossy(&add_output.stdout);
    let stderr = String::from_utf8_lossy(&add_output.stderr);
    assert!(
        add_output.status.success(),
        "aoe add --repo failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    assert!(
        stdout.contains("on_create hooks completed"),
        "should print hook completion message.\nstdout: {}",
        stdout
    );
    // #596: the merged hook commands should be printed before they run so the
    // user (especially with --trust-hooks) sees exactly what executes.
    assert!(
        stdout.contains("Running on_create hooks:")
            && stdout.contains(&format!("touch {}", hook_marker.display())),
        "should print the merged on_create command list.\nstdout: {}",
        stdout
    );
    assert!(
        hook_marker.exists(),
        "hook marker file should exist, proving on_create hooks ran"
    );
}

/// Regression test for #591: global hooks should execute as fallback when no
/// repo hooks are defined, even for workspace sessions.
#[test]
#[parallel]
fn test_cli_add_workspace_global_hook_fallback() {
    let h = TuiTestHarness::new("cli_workspace_global_hooks");

    let project_a = h.home_path().join("project-a");
    let project_b = h.home_path().join("project-b");
    init_git_repo(&project_a);
    init_git_repo(&project_b);

    // Set up global hooks (no repo config).
    let hook_marker = h.home_path().join("global-hook-ran.marker");
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[hooks]
on_create = ["touch {}"]
"#,
        env!("CARGO_PKG_VERSION"),
        hook_marker.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write global config");

    let add_output = h.run_cli(&[
        "add",
        project_a.to_str().unwrap(),
        "--repo",
        project_b.to_str().unwrap(),
        "-w",
        "feat/global-hook-test",
        "-b",
        "-t",
        "GlobalHookTest",
    ]);
    let stdout = String::from_utf8_lossy(&add_output.stdout);
    let stderr = String::from_utf8_lossy(&add_output.stderr);
    assert!(
        add_output.status.success(),
        "aoe add --repo failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    assert!(
        stdout.contains("on_create hooks completed"),
        "should print hook completion message for global hooks.\nstdout: {}",
        stdout
    );
    // #596: the merged list should surface the global fallback command too.
    assert!(
        stdout.contains("Running on_create hooks:")
            && stdout.contains(&format!("touch {}", hook_marker.display())),
        "should print the merged on_create command list for global hooks.\nstdout: {}",
        stdout
    );
    assert!(
        hook_marker.exists(),
        "global hook marker file should exist, proving global on_create hooks ran as fallback"
    );
}

/// #969: `aoe add -w <branch>` (without `-b`) should attach to an
/// already-existing worktree for that branch instead of bailing
/// because the computed path collides. Matches the TUI's
/// "Attach to existing branch" behavior.
#[test]
#[parallel]
fn test_cli_add_attaches_to_existing_worktree() {
    let h = TuiTestHarness::new("cli_attach_existing");
    let project = h.home_path().join("attach-project");
    init_git_repo(&project);

    // Create a worktree for `feat/existing` via the first `aoe add`.
    let first = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-w",
        "feat/existing",
        "-b",
        "-t",
        "FirstSession",
    ]);
    assert!(
        first.status.success(),
        "first aoe add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Second `aoe add -w feat/existing` (no `-b`) should attach to the
    // existing worktree, not bail.
    let second = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-w",
        "feat/existing",
        "-t",
        "SecondSession",
    ]);
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "second aoe add (attach) failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Attaching to existing worktree"),
        "expected 'Attaching to existing worktree' in stdout; got:\n{}",
        stdout
    );

    let json = read_sessions_json(&h);
    let sessions = json.as_array().expect("sessions array");
    let second_session = sessions
        .iter()
        .find(|s| s["title"].as_str() == Some("SecondSession"))
        .expect("SecondSession should exist");
    assert_eq!(
        second_session["worktree_info"]["managed_by_aoe"], false,
        "attached session should not own the worktree"
    );
    assert_eq!(
        second_session["worktree_info"]["branch"].as_str(),
        Some("feat/existing"),
    );
}

#[test]
#[parallel]
fn test_cli_add_sanitizes_explicit_worktree_branch() {
    let h = TuiTestHarness::new("cli_branch_sanitize");
    let project = h.home_path().join("branch-sanitize-project");
    init_git_repo(&project);

    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-w",
        "Exploration and issues v2",
        "-b",
        "-t",
        "SanitizedBranch",
    ]);
    assert!(
        add_output.status.success(),
        "aoe add with unsanitized explicit branch should succeed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let json = read_sessions_json(&h);
    let sessions = json.as_array().expect("sessions array");
    let session = sessions
        .iter()
        .find(|s| s["title"].as_str() == Some("SanitizedBranch"))
        .expect("SanitizedBranch session should exist");
    assert_eq!(
        session["worktree_info"]["branch"].as_str(),
        Some("Exploration-and-issues-v2"),
        "CLI should persist the same sanitized explicit branch name as the TUI/API path"
    );

    let branch = Command::new("git")
        .args(["branch", "--list", "Exploration-and-issues-v2"])
        .current_dir(&project)
        .output()
        .expect("git branch --list");
    assert!(
        String::from_utf8_lossy(&branch.stdout).contains("Exploration-and-issues-v2"),
        "sanitized branch should exist in the source repo"
    );
}

#[test]
#[parallel]
fn test_cli_add_blank_worktree_branch_falls_back_to_normal_title() {
    let h = TuiTestHarness::new("cli_blank_branch_title_fallback");
    let project = h.home_path().join("blank-branch-title-project");
    init_git_repo(&project);

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-w", "   ", "-b"]);
    assert!(
        add_output.status.success(),
        "blank explicit worktree branch should fall back instead of creating branch 'session':\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let json = read_sessions_json(&h);
    let sessions = json.as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1, "expected exactly one session");
    let session = &sessions[0];
    assert!(
        !session["title"].as_str().unwrap_or_default().is_empty(),
        "blank worktree branch should not become an empty session title"
    );
    assert!(
        session["worktree_info"].is_null(),
        "blank worktree branch should not create a managed worktree"
    );

    let branch = Command::new("git")
        .args(["branch", "--list", "session"])
        .current_dir(&project)
        .output()
        .expect("git branch --list");
    assert!(
        String::from_utf8_lossy(&branch.stdout).trim().is_empty(),
        "blank explicit branch should not create the sanitizer fallback branch"
    );
}

#[test]
#[parallel]
fn test_cli_add_scratch_provisions_dir() {
    let h = TuiTestHarness::new("cli_add_scratch");

    let add_output = h.run_cli(&["add", "--scratch", "-t", "QuickScratch"]);
    assert!(
        add_output.status.success(),
        "aoe add --scratch failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr),
    );
    let stdout = String::from_utf8_lossy(&add_output.stdout);
    assert!(
        stdout.contains("Scratch:") && stdout.contains("yes"),
        "expected scratch yes summary line; got:\n{}",
        stdout
    );

    let json = read_sessions_json(&h);
    let sessions = json.as_array().expect("sessions array");
    let session = sessions
        .iter()
        .find(|s| s["title"].as_str() == Some("QuickScratch"))
        .expect("QuickScratch must exist");
    assert_eq!(session["scratch"].as_bool(), Some(true));
    let project_path = session["project_path"]
        .as_str()
        .expect("project_path must be a string");
    let path = Path::new(project_path);
    assert!(path.exists(), "scratch dir must exist: {}", project_path);
    // Lives under <app_dir>/scratch/<id>/. The harness isolates app_dir
    // under its own temp tree, so we assert the structural shape rather than
    // a hard-coded location.
    assert!(
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("scratch"),
        "scratch dir must sit under a `scratch/` parent: {}",
        project_path
    );

    // Capture path before rm so we can assert cleanup.
    let captured = path.to_path_buf();

    // --purge: with trash-first delete (session.delete_to_trash default on,
    // #2489) a bare `rm` moves the session to the trash and keeps the scratch
    // dir; this test asserts the permanent cleanup path.
    let rm_output = h.run_cli(&["rm", "--purge", "QuickScratch"]);
    assert!(
        rm_output.status.success(),
        "aoe rm failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rm_output.stdout),
        String::from_utf8_lossy(&rm_output.stderr),
    );
    assert!(
        !captured.exists(),
        "scratch dir must be removed after aoe rm: {}",
        captured.display(),
    );
}

#[test]
#[parallel]
fn test_cli_add_scratch_rejects_explicit_path() {
    let h = TuiTestHarness::new("cli_add_scratch_path");
    let project = h.project_path();

    let output = h.run_cli(&["add", project.to_str().unwrap(), "--scratch"]);
    assert!(
        !output.status.success(),
        "aoe add <path> --scratch must error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cannot specify a project path with --scratch"),
        "unexpected error output:\n{}",
        stderr,
    );
}

#[test]
#[parallel]
fn test_cli_rm_keep_scratch_leaves_dir_on_disk() {
    let h = TuiTestHarness::new("cli_rm_keep_scratch");

    let add_output = h.run_cli(&["add", "--scratch", "-t", "KeepMe"]);
    assert!(add_output.status.success(), "aoe add --scratch failed");

    let json = read_sessions_json(&h);
    let session = json
        .as_array()
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|s| s["title"].as_str() == Some("KeepMe"))
        })
        .expect("KeepMe session must exist");
    let project_path = session["project_path"].as_str().expect("project_path");
    let path = Path::new(project_path).to_path_buf();
    assert!(path.exists(), "scratch dir must exist before rm");

    // --purge: keep-scratch is a permanent-delete option; with trash-first
    // (#2489) a bare `rm` would trash instead of purging, so pass --purge to
    // exercise the keep-scratch cleanup path.
    let rm_output = h.run_cli(&["rm", "--purge", "KeepMe", "--keep-scratch"]);
    assert!(
        rm_output.status.success(),
        "aoe rm --keep-scratch failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rm_output.stdout),
        String::from_utf8_lossy(&rm_output.stderr),
    );
    assert!(
        path.exists(),
        "scratch dir must survive when --keep-scratch is passed: {}",
        path.display(),
    );

    // The session record itself is gone.
    let json_after = read_sessions_json(&h);
    let still_there = json_after.as_array().and_then(|sessions| {
        sessions
            .iter()
            .find(|s| s["title"].as_str() == Some("KeepMe"))
    });
    assert!(
        still_there.is_none(),
        "session record must be removed even with --keep-scratch"
    );

    // Clean up the leftover dir so re-runs of this test don't accumulate
    // entries under the user's scratch root.
    let _ = std::fs::remove_dir_all(&path);
}

#[test]
#[parallel]
fn test_cli_add_scratch_conflicts_with_worktree_flag() {
    let h = TuiTestHarness::new("cli_add_scratch_worktree");

    let output = h.run_cli(&["add", "--scratch", "-w", "feat/x"]);
    assert!(
        !output.status.success(),
        "aoe add --scratch -w must error at clap layer"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // clap's mutex error wording mentions one of the two flag names.
    assert!(
        stderr.contains("--scratch")
            || stderr.contains("--worktree")
            || stderr.contains("cannot be used"),
        "unexpected error output:\n{}",
        stderr,
    );
}

/// #1051: `aoe ps --json` is substrate-agnostic and fail-soft. On an empty
/// profile with no tmux server it must emit an empty JSON array and exit 0,
/// never abort because a substrate probe failed.
#[test]
#[parallel]
fn test_cli_ps_empty_json() {
    let h = TuiTestHarness::new("cli_ps_empty_json");

    let output = h.run_cli(&["ps", "--json"]);
    assert!(
        output.status.success(),
        "aoe ps --json should succeed on an empty profile: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("aoe ps --json must emit valid JSON");
    assert!(
        value.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "expected an empty JSON array, got:\n{stdout}"
    );
}

/// `aoe stop` (with or without args) is a hidden trap, not a real command: it
/// must exit non-zero and redirect the user to the scoped stop verbs and
/// `killall` rather than silently doing nothing or triggering a teardown.
#[test]
#[parallel]
fn test_cli_stop_trap_redirects() {
    let h = TuiTestHarness::new("cli_stop_trap");
    for argv in [vec!["stop"], vec!["stop", "abc123"], vec!["stop", "--all"]] {
        let output = h.run_cli(&argv);
        assert!(!output.status.success(), "aoe {argv:?} must exit non-zero");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("aoe killall") && stderr.contains("aoe session stop"),
            "aoe {argv:?} should redirect to killall and session stop, got:\n{stderr}"
        );
    }
}

/// Fake agent that prints a substantial startup banner, sleeps to simulate a
/// slow-booting TUI, then prints opencode's real input-ready placeholder
/// text and reads exactly one line, echoing it back in a greppable form.
fn install_slow_boot_agent(h: &TuiTestHarness) -> std::path::PathBuf {
    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let script_path = dir.join("fake-slow-agent");
    let script = "#!/bin/sh\n\
echo '=== Fake Agent booting ==='\n\
echo 'Loading modules...'\n\
sleep 1\n\
echo '==========================='\n\
echo 'Ask anything...'\n\
read -r line\n\
echo \"GOT:[$line]\"\n\
sleep 60\n";
    std::fs::write(&script_path, script).expect("write fake agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake agent script");
    }
    script_path
}

/// `aoe send` right after `aoe session start` -- the sequence a headless
/// dispatcher with no controlling terminal must use, since it cannot rely on
/// `aoe add --launch`'s own attach -- must not race the agent's own boot: it
/// waits for a real per-agent readiness marker (`Session::wait_until_ready`,
/// `AgentDef::ready_marker`) before typing, instead of typing into a
/// still-booting pane whenever `ensure_pane_ready` reports `AlreadyAlive` (a
/// pane that already exists gets no wait at all otherwise). `--tool
/// opencode` exercises the real registered marker ("ask anything") rather
/// than the generic content-settle fallback. This test proves end-to-end
/// delivery still works; the deterministic proof that the wait actually
/// blocks until the marker appears (not just until the pane looks quiet)
/// lives in `wait_until_ready_blocks_until_the_marker_appears` in
/// `src/tmux/session.rs`, since canonical tty input buffering alone would
/// let a message typed before boot survive to `read -r` here regardless.
#[test]
#[parallel]
fn test_cli_send_waits_for_slow_boot_before_typing() {
    require_tmux!();

    let mut h = TuiTestHarness::new("cli_send_settle_wait");
    // Install an `opencode` shim so `is_agent_available` passes (the test
    // uses `--tool opencode` which triggers the built-in tool check).
    h.install_path_command("opencode");
    let fake_agent = install_slow_boot_agent(&h);
    let project = h.project_path();

    let add_output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "SlowBootSend",
        "--tool",
        "opencode",
        "--cmd-override",
        fake_agent.to_str().unwrap(),
    ]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session should have id")
        .to_string();

    let start_output = h.run_cli(&["session", "start", &session_id]);
    assert!(
        start_output.status.success(),
        "aoe session start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Send immediately: `aoe session start` returns as soon as the tmux pane
    // exists, well before the fake agent's 1s boot delay elapses, exactly
    // mirroring a headless dispatcher that has no controlling terminal to
    // attach with and so cannot use `aoe add --launch`'s readiness instead.
    let send_output = h.run_cli(&["send", &session_id, "hello there"]);
    assert!(
        send_output.status.success(),
        "aoe send failed: {}",
        String::from_utf8_lossy(&send_output.stderr)
    );

    // Poll the pane for the echoed line: it only appears once the fake
    // agent's `read -r line` has actually returned, proving the message was
    // delivered end to end rather than lost during boot.
    let sock = h.home_path().join("tmux.sock");
    let truncated_id = &session_id[..8.min(session_id.len())];
    let tmux_name = format!(
        "{}SlowBootSend_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );
    let mut content = String::new();
    for _ in 0..50 {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&sock)
            .args(["capture-pane", "-t", &format!("{tmux_name}:^.0"), "-p"])
            .output();
        if let Ok(out) = out {
            content = String::from_utf8_lossy(&out.stdout).to_string();
            if content.contains("GOT:[hello there]") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        content.contains("GOT:[hello there]"),
        "expected the fake agent to echo the sent message; pane content:\n{content}"
    );

    let _ = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .args(["kill-session", "-t", &tmux_name])
        .output();
}

/// #3350: the `aoe list --state` contract a scripted consumer relies on.
/// A trashed session must report `state: "trashed"` with a `trashed_at`
/// stamp, must be excluded by `--state=live`, and the resulting empty
/// listing must still be `[]` on the `--json` path rather than the
/// human "No sessions found" line, which a `jq` consumer cannot parse.
#[test]
#[parallel]
fn test_cli_list_state_filter_and_json_shape() {
    let h = TuiTestHarness::new("cli_list_state");
    let project = h.project_path();

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "State Probe"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let live: serde_json::Value =
        serde_json::from_slice(&h.run_cli(&["list", "--json", "--state=live"]).stdout)
            .expect("list --json --state=live must emit JSON");
    assert_eq!(live[0]["state"], "live");
    assert!(live[0].get("trashed_at").is_none());

    let rm = h.run_cli(&["rm", "State Probe"]);
    assert!(
        rm.status.success(),
        "aoe rm failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    let all: serde_json::Value = serde_json::from_slice(&h.run_cli(&["list", "--json"]).stdout)
        .expect("list --json must emit JSON");
    assert_eq!(all[0]["state"], "trashed");
    assert!(all[0]["trashed_at"].is_string());

    let out = h.run_cli(&["list", "--json", "--state=live"]);
    let empty: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "list --json with no matching rows must emit `[]`, got {:?} ({e})",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(empty, serde_json::json!([]));

    let trashed: serde_json::Value =
        serde_json::from_slice(&h.run_cli(&["list", "--json", "--state=trashed"]).stdout)
            .expect("list --json --state=trashed must emit JSON");
    // Assert the row's state, not just its title: with a single-row fixture a
    // title-only check would still pass if `--state=trashed` were ignored and
    // the live row returned instead. The state field pins the filter contract.
    assert_eq!(trashed[0]["title"], "State Probe");
    assert_eq!(trashed[0]["state"], "trashed");
    assert!(trashed[0]["trashed_at"].is_string());
}

/// Companion to `test_cli_list_state_filter_and_json_shape` for the lookup
/// path: a consumer that already holds the id should not have to fall back to
/// a full `aoe list --json` to learn whether that session is still around.
#[test]
#[parallel]
fn test_cli_session_show_json_carries_state() {
    let h = TuiTestHarness::new("cli_show_state");
    let project = h.project_path();

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Show Probe"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let live: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Show Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON");
    assert_eq!(live["state"], "live");
    assert!(live.get("trashed_at").is_none());
    assert!(live.get("archived_at").is_none());

    let rm = h.run_cli(&["rm", "Show Probe"]);
    assert!(
        rm.status.success(),
        "aoe rm failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    let trashed: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Show Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON for a trashed row");
    assert_eq!(trashed["state"], "trashed");
    assert!(trashed["trashed_at"].is_string());
}

/// The archived half of the same contract, and the precedence between the two
/// states: `trash()` deliberately keeps `archived_at`, so a row that was
/// archived first must still report `trashed`.
#[test]
#[parallel]
fn test_cli_session_show_json_reports_archived_and_precedence() {
    let h = TuiTestHarness::new("cli_show_archived");
    let project = h.project_path();

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Archive Probe"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let archive = h.run_cli(&["session", "archive", "Archive Probe"]);
    assert!(
        archive.status.success(),
        "aoe session archive failed: {}",
        String::from_utf8_lossy(&archive.stderr)
    );

    let archived: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Archive Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON for an archived row");
    assert_eq!(archived["state"], "archived");
    assert!(archived["archived_at"].is_string());
    assert!(archived.get("trashed_at").is_none());

    let rm = h.run_cli(&["rm", "Archive Probe"]);
    assert!(
        rm.status.success(),
        "aoe rm failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    let both: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Archive Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON for an archived, trashed row");
    assert_eq!(
        both["state"], "trashed",
        "trashed outranks archived, and archived_at survives the trash"
    );
    assert!(both["trashed_at"].is_string());
    assert!(both["archived_at"].is_string());
}

/// #3267 regression guard at the wiring level: `aoe acp doctor` must run
/// its version-gate probe on configured agents, so an adapter that is
/// present but below-floor reads `[!! ]` with remediation instead of
/// `[OK]`. Deleting the probe call from the listing loop fails here even
/// though every unit-level decision test stays green.
#[test]
#[parallel]
fn test_cli_acp_doctor_flags_below_floor_adapter() {
    let mut h = TuiTestHarness::new("cli_acp_doctor_below_floor");
    let bin = h.home_path().join("fixture-bin");
    std::fs::create_dir_all(&bin).expect("create fixture bin dir");
    let script = bin.join("claude-agent-acp");
    std::fs::write(&script, "#!/bin/sh\necho 0.37.0\n").expect("write fixture script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture executable");
    }
    h.add_path_dir(&bin);

    let out = h.run_cli(&["acp", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[!! ] claude"), "{stdout}");
    assert!(stdout.contains("installed 0.37.0; requires >="), "{stdout}");
    assert!(
        stdout.contains("npm install -g @agentclientprotocol/claude-agent-acp@latest"),
        "{stdout}"
    );
    assert!(!stdout.contains("[OK] claude"), "{stdout}");
}

/// Seed the pinned-bundle location `bundled_adapter_bin` resolves, with
/// a fixture reporting `version`.
fn seed_bundled_fixture(h: &TuiTestHarness, version: &str) {
    let bin = h.home_path().join(
        ".config/agent-of-empires-dev/acp-worker/adapters/claude-agent-acp/node_modules/.bin",
    );
    std::fs::create_dir_all(&bin).expect("create bundle bin dir");
    let script = bin.join("claude-agent-acp");
    std::fs::write(&script, format!("#!/bin/sh\necho {version}\n")).expect("write bundle fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make bundle fixture executable");
    }
}

/// A floor-COMPLIANT pinned bundled copy silences a stale PATH copy,
/// because spawn switches to the bundle below-floor. This is the only
/// end-to-end view of `bundled_copy_installed` +
/// `bundled_copy_meets_floor`: unit tests inject that flag directly.
#[test]
#[parallel]
fn test_cli_acp_doctor_compliant_bundle_silences_stale_path() {
    let mut h = TuiTestHarness::new("cli_acp_doctor_bundle_ok");
    let bin = h.home_path().join("fixture-bin");
    std::fs::create_dir_all(&bin).expect("create fixture bin dir");
    let script = bin.join("claude-agent-acp");
    std::fs::write(&script, "#!/bin/sh\necho 0.37.0\n").expect("write path fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make path fixture executable");
    }
    h.add_path_dir(&bin);
    seed_bundled_fixture(&h, "0.65.0");

    let out = h.run_cli(&["acp", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[OK] claude"), "{stdout}");
    assert!(!stdout.contains("[!! ] claude"), "{stdout}");
}

/// The mirror case: an installed but STALE pinned copy (below today's
/// floor) must not silence the listing, since spawn picks it over the
/// stale PATH copy and validate() rejects its handshake.
#[test]
#[parallel]
fn test_cli_acp_doctor_stale_bundle_keeps_flagging() {
    let mut h = TuiTestHarness::new("cli_acp_doctor_bundle_stale");
    let bin = h.home_path().join("fixture-bin");
    std::fs::create_dir_all(&bin).expect("create fixture bin dir");
    let script = bin.join("claude-agent-acp");
    std::fs::write(&script, "#!/bin/sh\necho 0.37.0\n").expect("write path fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make path fixture executable");
    }
    h.add_path_dir(&bin);
    seed_bundled_fixture(&h, "0.44.0");

    let out = h.run_cli(&["acp", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[!! ] claude"), "{stdout}");
    assert!(!stdout.contains("[OK] claude"), "{stdout}");
}

/// Bundle-ONLY installs (nothing on PATH) are invisible to the PATH
/// probe, so the pinned copy itself decides the verdict: compliant
/// reads `[OK]`, stale reads `[!! ]` naming the bundled version. This
/// is the stranded-pin cell after a floor bump.
#[test]
#[parallel]
fn test_cli_acp_doctor_bundle_only_judged_by_pinned_copy() {
    // (bundled fixture version, expected mark)
    let cases = [("0.65.0", "[OK] claude"), ("0.44.0", "[!! ] claude")];
    for (bundle_version, expected_mark) in cases {
        let mut h = TuiTestHarness::new("cli_acp_doctor_bundle_only");
        // The host PATH must not decide this cell: a global adapter
        // would route the listing through the PATH-present branch.
        // Scrubbing it leaves only the pinned copy visible, which is
        // exactly the bundle-only state under test; every invocation
        // here uses absolute paths, so nothing else needs PATH.
        h.set_env("PATH", "");
        seed_bundled_fixture(&h, bundle_version);

        let out = h.run_cli(&["acp", "doctor"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(expected_mark), "{bundle_version}: {stdout}");
    }
}

/// #3415: the snooze half of the four-timestamp set. `aoe session snooze`
/// is the only CLI producer of the marker, so this exercises the real path:
/// a live row omits both new keys, and after a snooze both surfaces carry
/// `snoozed_until` while staying `state: "live"` with no `pinned_at` (pin
/// has no CLI producer, it is set from the web sidebar).
#[test]
#[parallel]
fn test_cli_snoozed_row_exposes_snoozed_until() {
    let h = TuiTestHarness::new("cli_show_snoozed");
    let project = h.project_path();

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Snooze Probe"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let live: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Snooze Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON");
    assert!(live.get("snoozed_until").is_none());
    assert!(live.get("pinned_at").is_none());

    let snooze = h.run_cli(&["session", "snooze", "Snooze Probe", "--minutes", "5"]);
    assert!(
        snooze.status.success(),
        "aoe session snooze failed: {}",
        String::from_utf8_lossy(&snooze.stderr)
    );

    let listed: serde_json::Value = serde_json::from_slice(&h.run_cli(&["list", "--json"]).stdout)
        .expect("list --json must emit JSON");
    assert_eq!(listed[0]["state"], "live");
    assert!(listed[0]["snoozed_until"].is_string());
    assert!(listed[0].get("pinned_at").is_none());

    let shown: serde_json::Value = serde_json::from_slice(
        &h.run_cli(&["session", "show", "Snooze Probe", "--json"])
            .stdout,
    )
    .expect("session show --json must emit JSON for a snoozed row");
    assert_eq!(shown["state"], "live");
    assert!(shown["snoozed_until"].is_string());
    assert!(shown.get("pinned_at").is_none());
}

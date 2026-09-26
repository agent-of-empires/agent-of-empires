//! `aoe` CLI subcommands as plain subprocesses against an isolated home.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{
    app_dir_in, init_git_repo, require_tmux, wait_until, write_executable, TuiTestHarness,
};

fn json_out(h: &TuiTestHarness, args: &[&str]) -> Value {
    let stdout = h.run_cli_ok(args);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("aoe {args:?} must emit JSON: {e}"))
}

/// The tmux session name aoe derives for a session.
fn tmux_name(session_id: &str, title: &str) -> String {
    format!(
        "{}{title}_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        &session_id[..8.min(session_id.len())]
    )
}

#[test]
#[parallel]
fn cli_add_lists_the_new_session_and_names_the_binary_in_next_steps() {
    let h = TuiTestHarness::new("cli_add_list");
    let project = h.project_path();

    // #848: the hint must name `aoe`, not the long project name.
    let added = h.run_cli_ok(&["add", project.to_str().unwrap(), "-t", "NextSteps"]);
    assert!(added.contains("aoe session start NextSteps"), "{added}");
    assert!(!added.contains("agent-of-empires session start"), "{added}");

    assert!(h.run_cli_ok(&["list"]).contains("NextSteps"));
}

/// #3224: a repeated `aoe add` for the same title and path is a conflict, and
/// must not silently retain the first session's command override.
#[test]
#[parallel]
fn cli_add_duplicate_errors_and_preserves_original_command() {
    let h = TuiTestHarness::new("cli_add_duplicate");
    let project = h.project_path();
    let project = project.to_str().unwrap();
    let args = |cmd| {
        vec![
            "add",
            project,
            "--title",
            "Duplicate Session",
            "--cmd-override",
            cmd,
        ]
    };

    h.run_cli_ok(&args("echo OLD"));
    let duplicate = h.run_cli(&args("echo NEW"));
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

    let sessions = h.read_sessions();
    let sessions = sessions.as_array().expect("sessions array");
    assert_eq!(
        sessions.len(),
        1,
        "duplicate add must not create a second row"
    );
    assert_eq!(sessions[0]["command"], "echo OLD");
}

/// `aoe add` resolves the tool, command, and flags from config, with CLI flags
/// winning over config.
#[test]
#[parallel]
fn cli_add_resolves_tool_and_command_from_config() {
    struct Case {
        name: &'static str,
        config: &'static str,
        /// Installed on PATH before the add, for availability checks.
        path_command: Option<&'static str>,
        args: &'static [&'static str],
        expect: Vec<(&'static str, Value)>,
    }
    let cases = vec![
        Case {
            name: "no config picks the only available tool",
            config: "",
            path_command: None,
            args: &[],
            expect: vec![("tool", json!("claude"))],
        },
        Case {
            name: "default_tool sets tool and command",
            config: "[session]\ndefault_tool = \"opencode\"",
            path_command: None,
            args: &[],
            expect: vec![("tool", json!("opencode")), ("command", json!("opencode"))],
        },
        Case {
            name: "--cmd overrides default_tool",
            config: "[session]\ndefault_tool = \"opencode\"",
            path_command: None,
            args: &["--cmd", "claude"],
            expect: vec![("tool", json!("claude"))],
        },
        Case {
            name: "config extra args and command override",
            config: "[session]\ndefault_tool = \"claude\"\n\
                     agent_extra_args = { claude = \"--verbose --debug\" }\n\
                     agent_command_override = { claude = \"my-custom-claude\" }",
            path_command: None,
            args: &[],
            expect: vec![
                ("extra_args", json!("--verbose --debug")),
                ("command", json!("my-custom-claude")),
            ],
        },
        Case {
            name: "CLI flags beat config",
            config: "[session]\ndefault_tool = \"claude\"\n\
                     agent_extra_args = { claude = \"--from-config\" }\n\
                     agent_command_override = { claude = \"config-claude\" }",
            path_command: None,
            args: &[
                "--extra-args",
                "from-cli-extra",
                "--cmd-override",
                "cli-claude",
            ],
            expect: vec![
                ("extra_args", json!("from-cli-extra")),
                ("command", json!("cli-claude")),
            ],
        },
        Case {
            // #1910: availability checks the override binary, not the bare `qwen`.
            name: "--cmd availability uses the override binary",
            config: "[session]\nagent_command_override = { qwen = \"qwen-plannotator\" }",
            path_command: Some("qwen-plannotator"),
            args: &["--cmd", "qwen"],
            expect: vec![
                ("tool", json!("qwen")),
                ("command", json!("qwen-plannotator")),
            ],
        },
        Case {
            name: "yolo_mode_default",
            config: "[session]\nyolo_mode_default = true",
            path_command: None,
            args: &[],
            expect: vec![("yolo_mode", json!(true))],
        },
        Case {
            name: "--yolo flag",
            config: "",
            path_command: None,
            args: &["--yolo"],
            expect: vec![("yolo_mode", json!(true))],
        },
        Case {
            name: "custom agent keeps its command, extra args, and detect_as",
            config: "[session]\ncustom_agents = { custom = \"bash -lc true\" }\n\
                     agent_detect_as = { custom = \"claude\" }",
            path_command: None,
            args: &["--tool", "custom", "--extra-args", "--flag value"],
            expect: vec![
                ("tool", json!("custom")),
                ("command", json!("bash -lc true")),
                ("extra_args", json!("--flag value")),
                ("detect_as", json!("claude")),
            ],
        },
        Case {
            name: "custom agent without a detect_as mapping",
            config: "[session]\ncustom_agents = { custom = \"bash -lc true\" }",
            path_command: None,
            args: &["--tool", "custom"],
            expect: vec![("tool", json!("custom")), ("detect_as", json!(""))],
        },
        Case {
            name: "built-in --tool accepts --cmd-override",
            config: "",
            path_command: None,
            args: &["--tool", "claude", "--cmd-override", "custom-claude"],
            expect: vec![
                ("tool", json!("claude")),
                ("command", json!("custom-claude")),
            ],
        },
    ];

    for case in cases {
        let mut h = TuiTestHarness::new("cli_add_config");
        if let Some(command) = case.path_command {
            h.install_path_command(command);
        }
        if !case.config.is_empty() {
            h.append_config(case.config);
        }
        let project = h.project_path();
        let mut args = vec!["add", project.to_str().unwrap(), "-t", "ConfigCase"];
        args.extend_from_slice(case.args);
        h.run_cli_ok(&args);

        let sessions = h.read_sessions();
        let session = &sessions[0];
        for (field, expected) in case.expect {
            let actual = match expected {
                Value::Bool(_) => json!(session[field].as_bool()),
                _ => json!(session[field].as_str().unwrap_or("")),
            };
            let expected = match expected {
                Value::Bool(b) => json!(Some(b)),
                other => other,
            };
            assert_eq!(actual, expected, "{}: {field}", case.name);
        }
    }
}

/// `aoe add` rejects bad requests before writing anything, without leaking
/// configured command strings.
#[test]
#[parallel]
fn cli_add_rejects_invalid_requests() {
    struct Case {
        name: &'static str,
        config: &'static str,
        /// `$PROJECT` is replaced with the harness project path.
        args: &'static [&'static str],
        expect_any: &'static [&'static str],
        forbid: Option<&'static str>,
    }
    let cases = [
        Case {
            name: "nonexistent path",
            config: "",
            args: &["add", "/nonexistent/path/that/does/not/exist"],
            expect_any: &["not", "exist", "No such", "error", "Error", "invalid"],
            forbid: None,
        },
        Case {
            name: "unknown tool names safe alternatives only",
            config: "[session]\ncustom_agents = { custom = \"secret-command-for-leak-check\" }",
            args: &["add", "$PROJECT", "--tool", "missing"],
            expect_any: &["custom", "claude"],
            forbid: Some("secret-command-for-leak-check"),
        },
        Case {
            name: "custom tool rejects --cmd-override",
            config: "[session]\ncustom_agents = { custom = \"bash -lc true\" }",
            args: &[
                "add",
                "$PROJECT",
                "--tool",
                "custom",
                "--cmd-override",
                "other",
            ],
            expect_any: &[],
            forbid: None,
        },
        Case {
            name: "empty custom-agent command",
            config: "[session]\ncustom_agents = { custom = \"\" }",
            args: &["add", "$PROJECT", "--tool", "custom"],
            expect_any: &["empty"],
            forbid: None,
        },
        Case {
            name: "invalid detect_as target",
            config: "[session]\ncustom_agents = { custom = \"bash -lc true\" }\n\
                     agent_detect_as = { custom = \"not-a-built-in\" }",
            args: &["add", "$PROJECT", "--tool", "custom"],
            expect_any: &["agent_detect_as", "not-a-built-in"],
            forbid: None,
        },
        Case {
            name: "--tool conflicts with --cmd",
            config: "",
            args: &["add", "$PROJECT", "--tool", "custom", "--cmd", "claude"],
            expect_any: &["--tool", "--cmd"],
            forbid: None,
        },
        Case {
            name: "--scratch rejects an explicit path",
            config: "",
            args: &["add", "$PROJECT", "--scratch"],
            expect_any: &["Cannot specify a project path with --scratch"],
            forbid: None,
        },
        Case {
            name: "--scratch conflicts with --worktree",
            config: "",
            args: &["add", "--scratch", "-w", "feat/x"],
            expect_any: &["--scratch", "--worktree", "cannot be used"],
            forbid: None,
        },
        Case {
            // #1909: the guard must fail loudly rather than hang on the prompt.
            name: "--interactive without a TTY",
            config: "",
            args: &["add", "$PROJECT", "-i"],
            expect_any: &["requires a terminal"],
            forbid: None,
        },
    ];

    for case in cases {
        let h = TuiTestHarness::new("cli_add_invalid");
        if !case.config.is_empty() {
            h.append_config(case.config);
        }
        let project = h.project_path();
        let args: Vec<&str> = case
            .args
            .iter()
            .map(|a| {
                if *a == "$PROJECT" {
                    project.to_str().unwrap()
                } else {
                    a
                }
            })
            .collect();

        let out = h.run_cli(&args);
        assert!(!out.status.success(), "{}: must exit non-zero", case.name);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            case.expect_any.is_empty() || case.expect_any.iter().any(|n| combined.contains(n)),
            "{}: expected one of {:?} in:\n{combined}",
            case.name,
            case.expect_any
        );
        if let Some(secret) = case.forbid {
            assert!(!combined.contains(secret), "{}: leaked {secret}", case.name);
        }
        assert!(
            !h.sessions_path().exists(),
            "{}: must bail before writing sessions.json",
            case.name
        );
    }
}

/// #1909: `aoe add --interactive` prompts for a name over a real terminal and
/// persists what was typed.
#[test]
#[parallel]
fn cli_add_interactive_prompts_for_name() {
    require_tmux!();
    let mut h = TuiTestHarness::new("cli_add_interactive_prompt");
    let project = h.project_path();
    let project_arg = project.to_str().unwrap().to_string();

    h.spawn(&["add", &project_arg, "-i"]);
    h.wait_for("Session name [");
    h.type_text("InteractivePrompted");
    h.send_keys("Enter");

    // The pane dies with the command, so poll the store rather than the screen.
    wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        let sessions = h.try_read_sessions();
        sessions
            .as_array()
            .is_some_and(|rows| {
                rows.iter()
                    .any(|s| s["title"].as_str() == Some("InteractivePrompted"))
            })
            .then_some(())
            .ok_or_else(|| format!("no InteractivePrompted row yet: {sessions}"))
    });
}

/// #1996: `aoe mcp list --json` merges the native and global layers with
/// per-server provenance (global wins a collision) and reports secret env by
/// name only. A Codex entry with `enabled = false` stays out of the effective
/// set while remaining known to drift tracking.
#[test]
#[parallel]
fn cli_mcp_list_merges_layers_and_redacts_secrets() {
    let h = TuiTestHarness::new("cli_mcp_list");
    let home = h.home_path();
    std::fs::write(
        home.join(".claude.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-native" },
            "native-only": { "command": "n", "env": { "TOKEN": "SUPER_SECRET_DO_NOT_LEAK" } }
        } }"#,
    )
    .expect("write .claude.json");
    std::fs::write(
        app_dir_in(home).join("mcp.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-global" },
            "global-only": { "command": "g" }
        } }"#,
    )
    .expect("write mcp.json");

    let stdout = h.run_cli_ok(&["mcp", "list", "--agent", "claude", "--json"]);
    assert!(
        !stdout.contains("SUPER_SECRET_DO_NOT_LEAK"),
        "secret env value leaked to CLI output:\n{stdout}"
    );
    let val: Value = serde_json::from_str(&stdout).expect("output is JSON");
    let effective = val["effective"].as_array().expect("effective array");
    assert_eq!(effective.len(), 3, "native + global union, got {stdout}");
    let server = |name: &str| {
        effective
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("{name} present"))
            .clone()
    };
    assert_eq!(server("shared")["command"], "from-global");
    assert_eq!(server("shared")["provenance"], "global");
    assert_eq!(server("native-only")["provenance"], "agent-native:claude");
    assert_eq!(server("native-only")["envNames"], json!(["TOKEN"]));

    // The Codex phase asserts the native layer alone.
    std::fs::remove_file(app_dir_in(home).join("mcp.json")).expect("remove global mcp.json");
    let codex_dir = home.join(".codex");
    std::fs::create_dir_all(&codex_dir).expect("create .codex dir");
    std::fs::write(
        codex_dir.join("config.toml"),
        "[mcp_servers.omitted]\ncommand = \"omitted\"\n\n\
         [mcp_servers.explicit_true]\ncommand = \"true\"\nenabled = true\n\n\
         [mcp_servers.explicit_false]\ncommand = \"false\"\nenabled = false\n",
    )
    .expect("write Codex config");

    let val = json_out(&h, &["mcp", "list", "--agent", "codex", "--json"]);
    let names: Vec<&str> = val["effective"]
        .as_array()
        .expect("effective array")
        .iter()
        .map(|server| server["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["explicit_true", "omitted"]);
    assert_eq!(val["keptOnRemoval"], json!([]));
    assert_eq!(val["conflicts"], json!([]));
    assert_eq!(val["driftPaused"], false);
}

/// #3050: discover, adopt, edit, share, and remove a skill while the external
/// source stays untouched unless it is explicitly taken over.
#[test]
#[parallel]
fn cli_skill_management_flow() {
    let h = TuiTestHarness::new("cli_skill_management");
    let source = h.home_path().join(".claude/skills/review");
    std::fs::create_dir_all(&source).unwrap();
    let original = "---\nname: review\ndescription: Review code\n---\n\nOriginal body\n";
    std::fs::write(source.join("SKILL.md"), original).unwrap();
    let source_text = || std::fs::read_to_string(source.join("SKILL.md")).unwrap();

    let listed = json_out(&h, &["skill", "list", "--json"]);
    assert!(listed["skills"].as_array().unwrap().iter().any(|skill| {
        skill["directory"] == "review"
            && skill["provenance"]["root"] == "claude-user"
            && skill["provenance"]["kind"] == "external"
    }));

    h.run_cli_ok(&["skill", "adopt", "claude-user", "review"]);
    assert_eq!(source_text(), original, "adopt must not touch the source");

    let edited = "---\nname: review\ndescription: Updated\n---\n\nEdited body\n";
    let edit = h.run_cli_with_stdin(&["skill", "edit", "review", "--file", "-"], edited);
    assert!(
        edit.status.success(),
        "skill edit failed: {}",
        String::from_utf8_lossy(&edit.stderr)
    );
    assert_eq!(h.run_cli_ok(&["skill", "view", "review"]), edited);

    // Sync writes the managed skill into every agent's own skills dir.
    h.run_cli_ok(&["skill", "sync", "--json"]);
    for root in ["gemini/skills", "agents/skills", "config/opencode/skills"] {
        let landed = h.home_path().join(format!(".{root}/review/SKILL.md"));
        assert!(landed.is_file(), "{} missing after sync", landed.display());
    }
    let listed = json_out(&h, &["skill", "list", "--json"]);
    let sources: Vec<&str> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["directory"] == "review")
        .map(|s| s["provenance"]["root"].as_str().unwrap_or("aoe-managed"))
        .collect();
    assert_eq!(
        sources,
        vec!["aoe-managed", "claude-user"],
        "the copies AoE wrote must not be listed as more external skills"
    );

    // The user's own package is a conflict until `--replace` names it.
    let outcomes = json_out(&h, &["skill", "sync", "--root", "claude-user", "--json"]);
    assert_eq!(outcomes[0]["status"], "conflict", "{outcomes}");
    assert_eq!(source_text(), original);

    let outcomes = json_out(
        &h,
        &[
            "skill",
            "sync",
            "--root",
            "claude-user",
            "--replace",
            "review",
            "--json",
        ],
    );
    assert_eq!(outcomes[0]["status"], "updated", "{outcomes}");
    assert_eq!(source_text(), edited);

    // Removing the managed source and re-syncing withdraws every copy it made,
    // including the taken-over one, which is now AoE-owned.
    h.run_cli_ok(&["skill", "remove", "review"]);
    assert!(!app_dir_in(h.home_path()).join("skills/review").exists());
    h.run_cli_ok(&["skill", "sync"]);
    assert!(!h.home_path().join(".gemini/skills/review").exists());
    assert!(!source.exists());
}

/// `aoe stop` is a hidden trap that redirects to the scoped verbs rather than
/// silently doing nothing or tearing sessions down, and `aoe session capture`
/// reports a stopped session as empty in both output modes.
#[test]
#[parallel]
fn cli_stop_trap_redirects_and_capture_reports_a_stopped_session() {
    let h = TuiTestHarness::new("cli_capture_stopped");
    for argv in [vec!["stop"], vec!["stop", "abc123"], vec!["stop", "--all"]] {
        let stderr = h.run_cli_err(&argv);
        assert!(
            stderr.contains("aoe killall") && stderr.contains("aoe session stop"),
            "aoe {argv:?} should redirect to killall and session stop, got:\n{stderr}"
        );
    }

    let project = h.project_path();
    let session_id = h.add_session(&[project.to_str().unwrap(), "-t", "CaptureTest"]);
    let json = json_out(&h, &["session", "capture", &session_id, "--json"]);
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["content"], "");
    assert_eq!(json["title"], "CaptureTest");
    // Plain-text mode prints nothing for a stopped pane, and still exits 0.
    assert!(h
        .run_cli_ok(&["session", "capture", &session_id])
        .trim()
        .is_empty());
}

/// #3625: `aoe session capture` must run the profile's own
/// `[[agents.<name>.status_rules]]`, which the manifest-backed branch skipped,
/// so the CLI and the dashboard agree on a pane's status.
#[test]
#[parallel]
fn cli_session_capture_honors_configured_status_rules() {
    require_tmux!();
    let mut h = TuiTestHarness::new("cli_capture_rules");
    h.install_path_command("opencode");

    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let agent = dir.join("fake-rules-agent");
    // No opencode manifest rule reads this text as Waiting, so a Waiting
    // verdict can only come from the configured rule.
    write_executable(&agent, "#!/bin/sh\necho 'deploy to prod?'\nsleep 60\n");
    h.append_config(
        "[[agents.opencode.status_rules]]\nstatus = \"waiting\"\ncontains = \"deploy to prod?\"",
    );

    let project = h.project_path();
    let session_id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        "CaptureRules",
        "--tool",
        "opencode",
        "--cmd-override",
        agent.to_str().unwrap(),
    ]);
    h.run_cli_ok(&["session", "start", &session_id]);

    // The rule can only match once the pane has painted.
    let json = wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        let out = h.run_cli(&["session", "capture", &session_id, "--json"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
        if parsed["content"]
            .as_str()
            .is_some_and(|c| c.contains("deploy to prod?"))
        {
            Ok(parsed)
        } else {
            Err(format!("fake agent has not painted yet: {stdout}"))
        }
    });
    assert_eq!(
        json["status"], "waiting",
        "a configured status rule must decide what `aoe session capture` reports (#3625)"
    );
}

/// Renaming renames the agent's tmux session (#431) and removing kills it.
#[test]
#[parallel]
fn cli_rename_and_rm_move_the_agent_tmux_session() {
    require_tmux!();
    let h = TuiTestHarness::new("cli_rename_tmux");
    let project = h.project_path();
    let session_id = h.add_session(&[project.to_str().unwrap(), "-t", "OldName"]);

    let old_name = tmux_name(&session_id, "OldName");
    h.tmux_new_detached(&old_name, "sleep 60");

    h.run_cli_ok(&["session", "rename", &session_id, "-t", "NewName"]);
    let new_name = tmux_name(&session_id, "NewName");
    assert!(
        !h.tmux_has_session(&old_name),
        "{old_name} should be renamed away"
    );
    assert!(h.tmux_has_session(&new_name), "{new_name} should exist");

    h.run_cli_ok(&["rm", &session_id, "--force"]);
    assert!(
        !h.tmux_has_session(&new_name),
        "tmux session {new_name} should be gone after aoe rm"
    );
}

/// #591: `aoe add --repo` runs on_create hooks for workspace sessions, from
/// the repo config when it has them and from the global config otherwise, and
/// prints the merged command list before running it (#596).
#[test]
#[parallel]
fn cli_add_workspace_runs_repo_then_global_on_create_hooks() {
    for repo_hooks in [true, false] {
        let h = TuiTestHarness::new("cli_workspace_hooks");
        let project_a = h.home_path().join("project-a");
        let project_b = h.home_path().join("project-b");
        init_git_repo(&project_a);
        init_git_repo(&project_b);

        let marker = h.home_path().join("hook-ran.marker");
        let hooks = format!("[hooks]\non_create = [\"touch {}\"]", marker.display());
        if repo_hooks {
            let repo_config = project_a.join(".agent-of-empires");
            std::fs::create_dir_all(&repo_config).expect("create .agent-of-empires dir");
            std::fs::write(repo_config.join("config.toml"), &hooks).expect("write repo config");
        } else {
            h.append_config(&hooks);
        }

        let mut args = vec![
            "add",
            project_a.to_str().unwrap(),
            "--repo",
            project_b.to_str().unwrap(),
            "-w",
            "feat/hook-test",
            "-b",
            "-t",
            "HookTest",
        ];
        if repo_hooks {
            args.push("--trust-hooks");
        }
        let stdout = h.run_cli_ok(&args);
        assert!(stdout.contains("on_create hooks completed"), "{stdout}");
        assert!(
            stdout.contains("Running on_create hooks:")
                && stdout.contains(&format!("touch {}", marker.display())),
            "the merged command list must be printed before it runs.\n{stdout}"
        );
        assert!(
            marker.exists(),
            "repo_hooks={repo_hooks}: hooks did not run"
        );
    }
}

/// #969: `aoe add -w <branch>` without `-b` attaches to an existing worktree
/// rather than bailing on the path collision, and does not claim ownership.
#[test]
#[parallel]
fn cli_add_attaches_to_existing_worktree() {
    let h = TuiTestHarness::new("cli_attach_existing");
    let project = h.home_path().join("attach-project");
    init_git_repo(&project);
    let project = project.to_str().unwrap();

    h.run_cli_ok(&[
        "add",
        project,
        "-w",
        "feat/existing",
        "-b",
        "-t",
        "FirstSession",
    ]);
    let stdout = h.run_cli_ok(&["add", project, "-w", "feat/existing", "-t", "SecondSession"]);
    assert!(
        stdout.contains("Attaching to existing worktree"),
        "{stdout}"
    );

    let sessions = h.read_sessions();
    let second = crate::harness::session_by_title(&sessions, "SecondSession");
    assert_eq!(second["worktree_info"]["managed_by_aoe"], false);
    assert_eq!(
        second["worktree_info"]["branch"].as_str(),
        Some("feat/existing")
    );
}

/// The CLI sanitizes an explicit `-w` branch the way the TUI and API do, and a
/// blank one falls back to a normal titled session rather than a `session`
/// branch.
#[test]
#[parallel]
fn cli_add_sanitizes_or_ignores_the_explicit_worktree_branch() {
    for (branch, expected_branch) in [
        (
            "Exploration and issues v2",
            Some("Exploration-and-issues-v2"),
        ),
        ("   ", None),
    ] {
        let h = TuiTestHarness::new("cli_branch_sanitize");
        let project = h.home_path().join("branch-project");
        init_git_repo(&project);
        h.run_cli_ok(&[
            "add",
            project.to_str().unwrap(),
            "-w",
            branch,
            "-b",
            "-t",
            "Sanitized",
        ]);

        let sessions = h.read_sessions();
        let session = crate::harness::session_by_title(&sessions, "Sanitized");
        assert_eq!(
            session["worktree_info"]["branch"].as_str(),
            expected_branch,
            "branch {branch:?}"
        );

        let listed = Command::new("git")
            .args(["branch", "--list"])
            .current_dir(&project)
            .output()
            .expect("git branch --list");
        let listed = String::from_utf8_lossy(&listed.stdout);
        match expected_branch {
            Some(name) => assert!(listed.contains(name), "{name} missing from:\n{listed}"),
            None => assert!(
                !listed.contains("session"),
                "a blank branch must not create the sanitizer fallback branch:\n{listed}"
            ),
        }
    }
}

/// A scratch session provisions a dir under `<app_dir>/scratch/`, which
/// `rm --purge` removes and `--keep-scratch` leaves behind.
#[test]
#[parallel]
fn cli_scratch_session_provisions_and_purges_its_dir() {
    for keep_scratch in [false, true] {
        let h = TuiTestHarness::new("cli_add_scratch");
        let stdout = h.run_cli_ok(&["add", "--scratch", "-t", "QuickScratch"]);
        assert!(
            stdout.contains("Scratch:") && stdout.contains("yes"),
            "expected the scratch summary line; got:\n{stdout}"
        );

        let sessions = h.read_sessions();
        let session = crate::harness::session_by_title(&sessions, "QuickScratch");
        assert_eq!(session["scratch"].as_bool(), Some(true));
        let path = Path::new(session["project_path"].as_str().expect("project_path")).to_path_buf();
        assert!(path.exists(), "scratch dir must exist: {}", path.display());
        assert_eq!(
            path.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("scratch")),
            "scratch dir must sit under a `scratch/` parent: {}",
            path.display()
        );

        // A bare `rm` trashes the row (#2489), so purge to reach the cleanup.
        let mut args = vec!["rm", "--purge", "QuickScratch"];
        if keep_scratch {
            args.push("--keep-scratch");
        }
        h.run_cli_ok(&args);
        assert_eq!(
            path.exists(),
            keep_scratch,
            "keep_scratch={keep_scratch}: {}",
            path.display()
        );
        assert!(
            h.read_sessions()
                .as_array()
                .is_some_and(|rows| rows.is_empty()),
            "the session row goes even with --keep-scratch"
        );
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// #1051: `aoe ps --json` is fail-soft on an empty profile with no tmux
/// server: an empty array and exit 0, never a failed substrate probe.
#[test]
#[parallel]
fn cli_ps_empty_json() {
    let h = TuiTestHarness::new("cli_ps_empty_json");
    assert_eq!(json_out(&h, &["ps", "--json"]), json!([]));
}

/// `aoe send` straight after `aoe session start` (what a headless dispatcher
/// must do) waits for the agent's readiness marker instead of typing into a
/// still-booting pane. `--tool opencode` exercises the real registered marker;
/// that the wait blocks until the marker appears is unit-tested in
/// `src/tmux/session.rs`.
#[test]
#[parallel]
fn cli_send_waits_for_slow_boot_before_typing() {
    require_tmux!();
    let mut h = TuiTestHarness::new("cli_send_settle_wait");
    h.install_path_command("opencode");

    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let fake_agent = dir.join("fake-slow-agent");
    // Boots slowly, prints opencode's input-ready text, then echoes one line.
    write_executable(
        &fake_agent,
        "#!/bin/sh\n\
         echo '=== Fake Agent booting ==='\n\
         sleep 1\n\
         echo 'Ask anything...'\n\
         read -r line\n\
         echo \"GOT:[$line]\"\n\
         sleep 60\n",
    );

    let project = h.project_path();
    let session_id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        "SlowBootSend",
        "--tool",
        "opencode",
        "--cmd-override",
        fake_agent.to_str().unwrap(),
    ]);
    h.run_cli_ok(&["session", "start", &session_id]);
    // `session start` returns as soon as the pane exists, before the boot delay.
    h.run_cli_ok(&["send", &session_id, "hello there"]);

    // The echo only appears once the agent's `read -r line` returned.
    let pane = format!("{}:^.0", tmux_name(&session_id, "SlowBootSend"));
    wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        let out = h
            .tmux()
            .args(["capture-pane", "-t", &pane, "-p"])
            .output()
            .expect("capture-pane");
        let content = String::from_utf8_lossy(&out.stdout).to_string();
        if content.contains("GOT:[hello there]") {
            Ok(())
        } else {
            Err(format!("agent has not echoed the message:\n{content}"))
        }
    });
}

/// #3350 and #3415: the lifecycle state a scripted consumer reads from
/// `aoe list --json` and `aoe session show --json`. Snooze keeps a row live,
/// trashed outranks archived, `archived_at` survives the trash, and an empty
/// filtered listing is still `[]` rather than the human line.
#[test]
#[parallel]
fn cli_list_and_show_expose_lifecycle_state() {
    let h = TuiTestHarness::new("cli_list_state");
    let project = h.project_path();
    h.run_cli_ok(&["add", project.to_str().unwrap(), "-t", "State Probe"]);
    let show = |h: &TuiTestHarness| json_out(h, &["session", "show", "State Probe", "--json"]);

    let live = json_out(&h, &["list", "--json", "--state=live"]);
    assert_eq!(live[0]["state"], "live");
    assert!(live[0].get("trashed_at").is_none());
    let shown = show(&h);
    assert_eq!(shown["state"], "live");
    assert!(shown.get("snoozed_until").is_none());
    // `pinned_at` has no CLI producer; it is set from the web sidebar.
    assert!(shown.get("pinned_at").is_none());

    h.run_cli_ok(&["session", "snooze", "State Probe", "--minutes", "5"]);
    let snoozed = json_out(&h, &["list", "--json"]);
    assert_eq!(snoozed[0]["state"], "live");
    assert!(snoozed[0]["snoozed_until"].is_string());
    assert!(show(&h)["snoozed_until"].is_string());

    h.run_cli_ok(&["session", "archive", "State Probe"]);
    let archived = show(&h);
    assert_eq!(archived["state"], "archived");
    assert!(archived["archived_at"].is_string());
    assert!(archived.get("trashed_at").is_none());

    h.run_cli_ok(&["rm", "State Probe"]);
    let trashed = show(&h);
    assert_eq!(
        trashed["state"], "trashed",
        "trashed outranks archived, and archived_at survives the trash"
    );
    assert!(trashed["trashed_at"].is_string());
    assert!(trashed["archived_at"].is_string());

    let listed = json_out(&h, &["list", "--json", "--state=trashed"]);
    assert_eq!(listed[0]["title"], "State Probe");
    assert_eq!(listed[0]["state"], "trashed");
    assert_eq!(
        json_out(&h, &["list", "--json", "--state=live"]),
        json!([]),
        "an empty filtered listing must stay parseable"
    );
}

/// #3267: `aoe acp doctor` runs its version-gate probe on configured agents,
/// so a present-but-stale adapter reads `[!! ]` with remediation. The pinned
/// bundled copy decides the verdict whenever it is installed, because spawn
/// prefers it over a stale PATH copy.
#[test]
#[parallel]
fn cli_acp_doctor_flags_adapters_below_the_version_floor() {
    // (PATH adapter version, bundled adapter version, expected mark)
    let cases = [
        (Some("0.37.0"), None, "[!! ] claude"),
        (Some("0.37.0"), Some("0.65.0"), "[OK] claude"),
        (Some("0.37.0"), Some("0.44.0"), "[!! ] claude"),
        (None, Some("0.65.0"), "[OK] claude"),
        (None, Some("0.44.0"), "[!! ] claude"),
    ];
    for (path_version, bundle_version, expected) in cases {
        let label = format!("path={path_version:?} bundle={bundle_version:?}");
        let mut h = TuiTestHarness::new("cli_acp_doctor");
        match path_version {
            Some(version) => {
                let bin = h.home_path().join("fixture-bin");
                std::fs::create_dir_all(&bin).expect("create fixture bin dir");
                write_executable(
                    &bin.join("claude-agent-acp"),
                    &format!("#!/bin/sh\necho {version}\n"),
                );
                h.add_path_dir(&bin);
            }
            // A global adapter would route the listing through the
            // PATH-present branch; every call here uses absolute paths.
            None => h.set_env("PATH", ""),
        }
        if let Some(version) = bundle_version {
            let bin = h.home_path().join(
                ".config/agent-of-empires-dev/acp-worker/adapters/claude-agent-acp/node_modules/.bin",
            );
            std::fs::create_dir_all(&bin).expect("create bundle bin dir");
            write_executable(
                &bin.join("claude-agent-acp"),
                &format!("#!/bin/sh\necho {version}\n"),
            );
        }

        let stdout = String::from_utf8_lossy(&h.run_cli(&["acp", "doctor"]).stdout).into_owned();
        assert!(stdout.contains(expected), "{label}: {stdout}");
        let other = if expected.starts_with("[OK]") {
            "[!! ] claude"
        } else {
            "[OK] claude"
        };
        assert!(!stdout.contains(other), "{label}: {stdout}");
        if expected.starts_with("[!! ]") && path_version.is_some() && bundle_version.is_none() {
            assert!(stdout.contains("installed 0.37.0; requires >="), "{stdout}");
            assert!(
                stdout.contains("npm install -g @agentclientprotocol/claude-agent-acp@latest"),
                "{stdout}"
            );
        }
    }
}

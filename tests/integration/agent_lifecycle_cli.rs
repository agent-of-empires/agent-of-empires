#![cfg(unix)]
//!
//! Observable CLI contracts for the agent-lifecycle mechanism, proven against
//! the real binary (`CARGO_BIN_EXE_aoe`) with a PATH-stubbed `gemini` and an
//! isolated HOME:
//!
//! - `aoe agents` lists the deprecation notice under gemini;
//! - `aoe acp doctor` emits the notice line in text mode;
//! - `aoe add` emits exactly one non-blocking warning after every built-in
//!   resolution path, including default tools and command overrides.
//!
//! These pin the rendering sites themselves (`cli/agents.rs`, `cli/acp.rs`,
//! `cli/add.rs`); the shared notice producer is pinned separately in
//! `src/agents.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory holding executable stubs so PATH-based detection reports them
/// installed. The TempDir must stay alive for the run; dropping it removes
/// the stubs (tests clean up after themselves).
fn stub_dir(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).expect("stub tempdir");
    use std::os::unix::fs::PermissionsExt;
    for name in names {
        let path = dir.path().join(name);
        let mut stub = std::fs::File::create(&path).expect("create stub");
        writeln!(stub, "#!/bin/sh").unwrap();
        writeln!(stub, "echo 0.14.0").unwrap();
        drop(stub);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    }
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn isolated_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    (tmp, home, xdg)
}

fn write_config(xdg: &Path, session_toml: &str) {
    if session_toml.is_empty() {
        return;
    }
    let app = xdg.join(if cfg!(debug_assertions) {
        "agent-of-empires-dev"
    } else {
        "agent-of-empires"
    });
    std::fs::create_dir_all(&app).expect("create app dir");
    let config = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
last_seen_version = "{}"

[session]
{}
"#,
        env!("CARGO_PKG_VERSION"),
        session_toml
    );
    std::fs::write(app.join("config.toml"), config).expect("write config");
}

fn init_repo_isolated(repo: &Path, home: &Path, xdg: &Path) {
    let empty_global = home.join("empty-gitconfig");
    std::fs::write(&empty_global, "").expect("write empty gitconfig");
    let output = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty_global)
        .env("GIT_TEMPLATE_DIR", "")
        .output()
        .expect("git init");
    assert!(
        output.status.success(),
        "isolated git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_aoe(home: &Path, xdg: &Path, stub: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(args)
        .env(
            "PATH",
            format!(
                "{}:{}",
                stub.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        .output()
        .expect("run aoe")
}

#[test]
fn aoe_agents_lists_deprecated_notice() {
    let (_tmp, home, xdg) = isolated_dirs();
    let (_stub_tmp, stub) = stub_dir(&["gemini"]);
    let out = run_aoe(&home, &xdg, &stub, &["agents"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The ✓ mark carries its own color resets between glyph and name, so
    // the contract pins the notice itself, and exactly once: every Active
    // agent must stay notice-free.
    assert!(stdout.contains("⚠ deprecated since 2026-06-18"), "{stdout}");
    assert!(
        stdout.contains("consider switching to antigravity"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("\x1b["),
        "piped agents output must be escape-free"
    );
    assert_eq!(stdout.matches("deprecated since").count(), 1, "{stdout}");
}

#[test]
fn acp_doctor_text_emits_amber_lifecycle_notice() {
    let (_tmp, home, xdg) = isolated_dirs();
    let (_stub_tmp, stub) = stub_dir(&["gemini"]);
    let out = run_aoe(&home, &xdg, &stub, &["acp", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The stub makes command_present true; the notice must ride along on
    // the same row block. Captured output is piped, so it arrives as
    // plain text: color is tty-only by contract.
    assert!(stdout.contains("[OK] gemini"), "{stdout}");
    assert!(stdout.contains("⚠ deprecated since 2026-06-18"), "{stdout}");
    assert!(
        !stdout.contains("\x1b[33m"),
        "piped output must be escape-free"
    );
}

#[test]
fn aoe_add_lifecycle_warning_covers_all_resolution_paths() {
    struct Case {
        name: &'static str,
        args: &'static [&'static str],
        config: &'static str,
        stubs: &'static [&'static str],
        expected_warnings: usize,
    }

    let cases = [
        Case {
            name: "explicit tool",
            args: &["--tool", "gemini"],
            config: "",
            stubs: &["gemini"],
            expected_warnings: 1,
        },
        Case {
            name: "configured default tool",
            args: &[],
            config: "default_tool = \"gemini\"",
            stubs: &["gemini"],
            expected_warnings: 1,
        },
        Case {
            name: "command with configured override",
            args: &["--cmd", "gemini"],
            config: "agent_command_override = { gemini = \"gemini-wrapper\" }",
            stubs: &["gemini-wrapper"],
            expected_warnings: 1,
        },
        Case {
            name: "configured custom agent",
            args: &["--tool", "custom"],
            config: "custom_agents = { custom = \"bash -lc true\" }",
            stubs: &[],
            expected_warnings: 0,
        },
        Case {
            name: "command without override warns once",
            args: &["--cmd", "gemini"],
            config: "",
            stubs: &["gemini"],
            expected_warnings: 1,
        },
    ];

    for case in cases {
        let (tmp, home, xdg) = isolated_dirs();
        let (_stub_tmp, stub) = stub_dir(case.stubs);
        write_config(&xdg, case.config);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let hostile_template = tmp.path().join("hostile-template");
        std::fs::create_dir_all(&hostile_template).unwrap();
        std::fs::write(hostile_template.join("USER_TEMPLATE_LEAK"), "hostile").unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            format!("[init]\n\ttemplateDir = {}\n", hostile_template.display()),
        )
        .unwrap();
        init_repo_isolated(&repo, &home, &xdg);
        assert!(
            !repo.join(".git/USER_TEMPLATE_LEAK").exists(),
            "{}: git init inherited the hostile user template",
            case.name
        );
        let mut args = vec!["add", repo.to_str().unwrap()];
        args.extend_from_slice(case.args);
        let out = run_aoe(&home, &xdg, &stub, &args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let warnings = stderr
            .matches("Warning: gemini is deprecated since 2026-06-18")
            .count();
        assert_eq!(warnings, case.expected_warnings, "{}: {stderr}", case.name);
        assert!(
            stdout.contains("✓ Added session"),
            "{}: warning must stay non-blocking; stdout: {stdout}; stderr: {stderr}",
            case.name
        );
    }
}

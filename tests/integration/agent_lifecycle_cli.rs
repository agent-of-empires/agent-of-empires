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
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aoe"));
        command
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
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("AOE_TMUX_SOCKET", home.join("tmux.sock"))
            .env_remove("AOE_DAEMON_URL")
            .env_remove("AOE_NO_AUTOSTART");
        command
    };
    let output = command().args(args).output().expect("run aoe");
    if args.first() == Some(&"add") {
        let stopped = command()
            .args(["serve", "--stop"])
            .output()
            .expect("stop isolated Daemon");
        assert!(stopped.status.success(), "{stopped:?}");
    }
    output
}

/// `aoe agents` and `aoe acp doctor` both render the deprecation notice for
/// the stubbed gemini. Captured output is piped, so it arrives as plain text:
/// color is tty-only by contract.
#[test]
#[serial_test::parallel]
fn agents_and_acp_doctor_list_the_deprecated_notice() {
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
    assert!(!stdout.contains("\x1b["), "{stdout}");
    assert_eq!(stdout.matches("deprecated since").count(), 1, "{stdout}");

    // The stub makes command_present true; the notice rides on the same row.
    let out = run_aoe(&home, &xdg, &stub, &["acp", "doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[OK] gemini"), "{stdout}");
    assert!(stdout.contains("⚠ deprecated since 2026-06-18"), "{stdout}");
    assert!(!stdout.contains("\x1b[33m"), "{stdout}");
}

#[test]
#[serial_test::parallel]
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

#[test]
#[serial_test::parallel]
fn cli_builtin_launch_keeps_prompt_and_project_path_literal() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    if !Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }
    let (_tmp, home, xdg) = isolated_dirs();
    let (_stub_tmp, stub) = stub_dir(&["claude"]);
    struct LaunchCleanup<'a>(&'a Path, &'a Path, &'a Path);
    impl Drop for LaunchCleanup<'_> {
        fn drop(&mut self) {
            let _ = run_aoe(self.0, self.1, self.2, &["serve", "--stop"]);
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(self.0.join("tmux.sock"))
                .arg("kill-server")
                .output();
        }
    }
    let _cleanup = LaunchCleanup(&home, &xdg, &stub);
    write_config(
        &xdg,
        "default_tool = \"claude\"\n[systemd]\nenabled = false",
    );
    let profile = run_aoe(&home, &xdg, &stub, &["profile", "create", "default"]);
    assert!(profile.status.success(), "{profile:?}");
    let approved = run_aoe(&home, &xdg, &stub, &["hooks", "approve", "-p", "default"]);
    assert!(approved.status.success(), "{approved:?}");
    let observed = home.join("arguments");
    let cwd = home.join("cwd");
    let ready = home.join("ready");
    let injected = home.join("must-not-run");
    let script = stub.join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--help) printf '%s\\n' '--session-id --resume --fork-session --name'; exit 0;;\n--version) echo '2.1.0'; exit 0;;\nesac\nprintf '%s\\0' \"$@\" > {}\npwd > {}\n: > {}\nexec sleep 60\n",
            shell_words::quote(observed.to_str().unwrap()),
            shell_words::quote(cwd.to_str().unwrap()),
            shell_words::quote(ready.to_str().unwrap()),
        ),
    )
    .unwrap();
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prompt = format!(
        "Use --model $(:>{}) now",
        shell_words::quote(injected.to_str().unwrap())
    );
    let extra_args = format!("--append-system-prompt {}", shell_words::quote(&prompt));
    for (index, name) in ["plain-project", "repo[1]-Bob's-$(false)"]
        .into_iter()
        .enumerate()
    {
        let project = home.join(name);
        std::fs::create_dir_all(&project).unwrap();
        let output = run_aoe(
            &home,
            &xdg,
            &stub,
            &[
                "add",
                project.to_str().unwrap(),
                "-p",
                "default",
                "-t",
                &format!("literal-launch-{index}"),
                "--extra-args",
                &extra_args,
                "--launch",
            ],
        );
        assert!(output.status.success(), "{output:?}");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "agent never published argv: {output:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let bytes = std::fs::read(&observed).unwrap();
        let argv: Vec<_> = bytes
            .split(|byte| *byte == 0)
            .filter(|word| !word.is_empty())
            .map(|word| std::str::from_utf8(word).unwrap())
            .collect();
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--append-system-prompt", prompt.as_str()]),
            "{argv:?}"
        );
        assert!(
            !injected.exists(),
            "literal prompt executed a shell substitution"
        );
        assert_eq!(
            std::fs::read_to_string(&cwd).unwrap().trim_end(),
            project.canonicalize().unwrap().to_str().unwrap()
        );
        std::fs::remove_file(&ready).unwrap();
    }
}

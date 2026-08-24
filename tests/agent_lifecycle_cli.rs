#![cfg(unix)]
//!
//! Observable CLI contracts for the agent-lifecycle mechanism, proven against
//! the real binary (`CARGO_BIN_EXE_aoe`) with a PATH-stubbed `gemini` and an
//! isolated HOME:
//!
//! - `aoe agents` lists the deprecation notice under gemini;
//! - `aoe acp doctor` (serve build) emits the amber notice line in text mode;
//! - `aoe add --tool gemini` warns on stderr and still creates the session,
//!   pinning the non-blocking contract end to end.
//!
//! These pin the rendering sites themselves (`cli/agents.rs`, `cli/acp.rs`,
//! `cli/add.rs`); the shared notice producer is pinned separately in
//! `src/agents.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory holding an executable `gemini` stub so PATH-based detection
/// reports it installed.
fn stub_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aoe-lifecycle-stub-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create stub dir");
    let mut gemini = std::fs::File::create(dir.join("gemini")).expect("create stub");
    writeln!(gemini, "#!/bin/sh").unwrap();
    writeln!(gemini, "echo 0.14.0").unwrap();
    drop(gemini);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("gemini"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub");
    dir
}

/// Isolated HOME/XDG so runs never touch real user state. The TempDir must
/// stay alive for the duration of the run.
fn isolated_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    (tmp, home, xdg)
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
    let stub = stub_dir();
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
    assert_eq!(stdout.matches("deprecated since").count(), 1, "{stdout}");
}

#[cfg(feature = "serve")]
#[test]
fn acp_doctor_text_emits_amber_lifecycle_notice() {
    let (_tmp, home, xdg) = isolated_dirs();
    let stub = stub_dir();
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
fn aoe_add_warns_but_still_creates_session() {
    let (tmp, home, xdg) = isolated_dirs();
    let stub = stub_dir();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .output()
        .expect("git init");
    let out = run_aoe(
        &home,
        &xdg,
        &stub,
        &["add", repo.to_str().unwrap(), "--tool", "gemini"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("Warning: gemini is deprecated since 2026-06-18"),
        "stderr: {stderr}"
    );
    assert!(
        stdout.contains("✓ Added session"),
        "warning must stay non-blocking; stdout: {stdout}"
    );
}

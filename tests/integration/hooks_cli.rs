//! #4159: the agent-hook acknowledgement gate must be clearable without the
//! TUI, and every launch path must be unblocked by the same consent.
//!
//! Pinned against the real binary on an isolated home and tmux socket:
//! an unacknowledged install refuses a host launch and names the command that
//! clears it; after that command the very same session launches.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Kills the tmux server on this test's socket, so launched panes do not leak.
struct TmuxCleanup(PathBuf);

impl Drop for TmuxCleanup {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.0)
            .arg("kill-server")
            .output();
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Fails {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Fails {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn run_aoe(home: &Path, xdg: &Path, stub: &Path, socket: &Path, args: &[&str]) -> Fails {
    let out = Command::new(env!("CARGO_BIN_EXE_aoe"))
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
        .env("AOE_TMUX_SOCKET", socket)
        .output()
        .expect("run aoe");
    Fails {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn session_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("ID:"))
        .expect("aoe add prints the session id")
        .trim()
        .to_string()
}

fn hook_path(home: &Path) -> PathBuf {
    home.join(".codex").join("hooks.json")
}

#[test]
fn hooks_approve_clears_the_launch_gate_for_every_path() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    let stub = tmp.path().join("stub");
    for dir in [&home, &xdg, &stub] {
        std::fs::create_dir_all(dir).expect("create dir");
    }
    // A codex that blocks keeps the launched pane alive. It must not create
    // the hook file: that file is AoE's to write, and the final assertion
    // below is only meaningful if the stub leaves it alone.
    let mut agent = std::fs::File::create(stub.join("codex")).expect("create stub");
    writeln!(agent, "#!/bin/sh\nsleep 300").unwrap();
    drop(agent);
    std::fs::set_permissions(stub.join("codex"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub");

    let socket = tmp.path().join("tmux.sock");
    let _cleanup = TmuxCleanup(socket.clone());

    // 1. The issue's command: refused, and the refusal says how to clear it.
    let add = run_aoe(
        &home,
        &xdg,
        &stub,
        &socket,
        &[
            "add",
            "--scratch",
            "--tool",
            "codex",
            "--trust-hooks",
            "--title",
            "gated",
            "-l",
        ],
    );
    assert_ne!(
        add.code,
        Some(0),
        "unapproved launch must fail: {}",
        add.all()
    );
    assert!(
        add.all().contains("aoe hooks approve"),
        "refusal must name the command that clears it: {}",
        add.all()
    );
    let id = session_id(&add.stdout);

    // 2. The retry the refusal itself suggests must be the command that works.
    let retry = run_aoe(&home, &xdg, &stub, &socket, &["session", "start", &id]);
    assert_ne!(
        retry.code,
        Some(0),
        "retry before approval must still fail: {}",
        retry.all()
    );
    assert!(
        retry.all().contains("aoe hooks approve"),
        "the suggested retry must stay refused until approved: {}",
        retry.all()
    );

    // Status must read the state, not assume it: the same command reported
    // "not approved" here and "approved" below.
    let before = run_aoe(&home, &xdg, &stub, &socket, &["hooks", "status"]);
    assert!(
        before.stdout.contains("not approved"),
        "status must report the unapproved install: {}",
        before.stdout
    );
    assert!(
        before
            .stdout
            .contains(&hook_path(&home).display().to_string()),
        "status must disclose the codex hook path before approval: {}",
        before.stdout
    );

    // 3. Approving discloses what would be written, then unblocks the launch.
    let approve = run_aoe(&home, &xdg, &stub, &socket, &["hooks", "approve"]);
    assert_eq!(
        approve.code,
        Some(0),
        "approve must succeed: {}",
        approve.all()
    );
    assert!(
        approve
            .all()
            .contains(&hook_path(&home).display().to_string()),
        "approval must disclose the codex hook path: {}",
        approve.all()
    );
    let status = run_aoe(&home, &xdg, &stub, &socket, &["hooks", "status"]);
    assert!(
        status.stdout.contains("approved for this installation"),
        "status must report the approval: {}",
        status.stdout
    );

    // 4. The session refused above now launches, and the hooks land where the
    //    approval said they would.
    let start = run_aoe(&home, &xdg, &stub, &socket, &["session", "start", &id]);
    assert_eq!(start.code, Some(0), "start after approval: {}", start.all());
    assert!(
        !start.all().contains("have not been acknowledged"),
        "gate must stay open: {}",
        start.all()
    );
    assert!(
        hook_path(&home).is_file(),
        "approved hook file must be written at the disclosed path"
    );
}

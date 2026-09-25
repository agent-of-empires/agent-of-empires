//! Integration tests for TUI attach/detach behavior
//!
//! These tests validate that the terminal state is properly managed when
//! attaching to and detaching from tmux sessions.

use std::process::Command;

/// Verify tmux is available for testing
fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Test that tmux sessions can be created and killed
#[test]
#[serial_test::parallel]
fn test_tmux_session_lifecycle() {
    if !tmux_available() {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    struct Server(tempfile::TempDir);
    impl Server {
        fn command(&self) -> Command {
            let mut command = Command::new("tmux");
            command.arg("-S").arg(self.0.path().join("tmux.sock"));
            command
                .env("HOME", self.0.path())
                .env("XDG_CONFIG_HOME", self.0.path().join(".config"));
            command
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.command().arg("kill-server").output();
        }
    }
    let server = Server(tempfile::tempdir().expect("private tmux server"));
    let session_name = "lifecycle";

    // Create a detached session
    let create = server
        .command()
        .args(["new-session", "-d", "-s", session_name])
        .output()
        .expect("Failed to create tmux session");

    assert!(create.status.success(), "Failed to create test session");

    // Verify session exists
    let check = server
        .command()
        .args(["has-session", "-t", session_name])
        .output()
        .expect("Failed to check session");

    assert!(
        check.status.success(),
        "Session should exist after creation"
    );

    // Kill session
    let kill = server
        .command()
        .args(["kill-session", "-t", session_name])
        .output()
        .expect("Failed to kill session");

    assert!(kill.status.success(), "Failed to kill test session");

    // Verify session no longer exists
    let check_after = server
        .command()
        .args(["has-session", "-t", session_name])
        .output()
        .expect("Failed to check session");

    assert!(
        !check_after.status.success(),
        "Session should not exist after kill"
    );
}

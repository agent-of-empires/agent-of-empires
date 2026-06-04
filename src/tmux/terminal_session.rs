//! Paired terminal sessions — host (`TerminalSession`) and sandbox (`ContainerTerminalSession`).
//!
//! The two session types have nearly identical lifecycles, so the
//! implementation lives in [`PairedTerminal`] and the public types are thin
//! wrappers that fix the tmux name prefix and the log-message label.

use anyhow::{bail, Result};
use std::process::Command;

use super::utils::{
    append_clipboard_passthrough_args, append_mouse_on_args, append_pane_base_index_args,
    append_remain_on_exit_args, append_window_size_args, is_pane_dead, sanitize_session_name,
};
use super::{
    refresh_session_cache, session_exists_from_cache, CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::config::should_apply_tmux_clipboard;

/// Classifies a paired terminal: adjusts the tmux session prefix and the
/// human-readable label used in error messages.
#[derive(Debug, Clone, Copy)]
enum TerminalKind {
    Host,
    Container,
}

impl TerminalKind {
    fn prefix(self) -> &'static str {
        match self {
            TerminalKind::Host => TERMINAL_PREFIX,
            TerminalKind::Container => CONTAINER_TERMINAL_PREFIX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TerminalKind::Host => "terminal session",
            TerminalKind::Container => "container terminal session",
        }
    }
}

/// Shared implementation of the paired-terminal lifecycle. Not exposed; the
/// public [`TerminalSession`] and [`ContainerTerminalSession`] wrap one of
/// these with a fixed [`TerminalKind`].
struct PairedTerminal {
    name: String,
    kind: TerminalKind,
}

impl PairedTerminal {
    fn generate_name(kind: TerminalKind, id: &str, title: &str) -> String {
        let safe_title = sanitize_session_name(title);
        format!("{}{}_{}", kind.prefix(), safe_title, truncate_id(id, 8))
    }

    fn new(kind: TerminalKind, id: &str, title: &str) -> Self {
        Self {
            name: Self::generate_name(kind, id, title),
            kind,
        }
    }

    fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        Command::new("tmux")
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }

        let mut args = build_terminal_create_args(&self.name, working_dir, command, size);
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

        let output = Command::new("tmux").args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "duplicate session" means a concurrent caller won the race;
            // the session exists now, which is what we wanted.
            if stderr.contains("duplicate session") {
                refresh_session_cache();
                return Ok(());
            }
            bail!("Failed to create {}: {}", self.kind.label(), stderr);
        }

        refresh_session_cache();

        Ok(())
    }

    fn kill(&self) -> Result<()> {
        if !self.exists() {
            return Ok(());
        }

        // Kill the entire process tree first to ensure child processes are terminated
        if let Some(pane_pid) = self.get_pane_pid() {
            process::kill_process_tree(pane_pid);
        }

        super::utils::kill_session_if_present(&self.name)?;

        refresh_session_cache();

        Ok(())
    }

    fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }

    fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("{} does not exist: {}", self.kind.label(), self.name);
        }

        if std::env::var("TMUX").is_ok() {
            let status = Command::new("tmux")
                .args(["switch-client", "-t", &self.name])
                .status()?;

            if !status.success() {
                let status = Command::new("tmux")
                    .args(["attach-session", "-t", &self.name])
                    .status()?;

                if !status.success() {
                    bail!("Failed to attach to {}", self.kind.label());
                }
            }
        } else {
            let status = Command::new("tmux")
                .args(["attach-session", "-t", &self.name])
                .status()?;

            if !status.success() {
                bail!("Failed to attach to {}", self.kind.label());
            }
        }

        Ok(())
    }

    fn capture_pane(&self, lines: usize) -> Result<String> {
        if !self.exists() {
            return Ok(String::new());
        }

        // Use `^.0` to target the first window's first pane regardless of
        // base-index or which pane is active.  See #435, #488.
        let target = format!("{}:^.0", self.name);
        let output = Command::new("tmux")
            .args([
                "capture-pane",
                "-t",
                &target,
                "-p",
                "-e",
                "-S",
                &format!("-{}", lines),
            ])
            .output()?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Ok(String::new())
        }
    }
}

pub struct TerminalSession {
    inner: PairedTerminal,
}

impl TerminalSession {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Host, id, title),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        PairedTerminal::generate_name(TerminalKind::Host, id, title)
    }

    pub fn exists(&self) -> bool {
        self.inner.exists()
    }

    pub fn is_pane_dead(&self) -> bool {
        self.inner.is_pane_dead()
    }

    pub fn create(&self, working_dir: &str) -> Result<()> {
        self.inner.create_with_size(working_dir, None, None)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        self.inner.create_with_size(working_dir, command, size)
    }

    pub fn kill(&self) -> Result<()> {
        self.inner.kill()
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        self.inner.get_pane_pid()
    }

    pub fn attach(&self) -> Result<()> {
        self.inner.attach()
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        self.inner.capture_pane(lines)
    }
}

/// Container terminal session for sandboxed sessions.
/// Uses a separate prefix (aoe_cterm_) to allow both container and host terminals to coexist.
pub struct ContainerTerminalSession {
    inner: PairedTerminal,
}

impl ContainerTerminalSession {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Container, id, title),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        PairedTerminal::generate_name(TerminalKind::Container, id, title)
    }

    pub fn exists(&self) -> bool {
        self.inner.exists()
    }

    pub fn is_pane_dead(&self) -> bool {
        self.inner.is_pane_dead()
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        self.inner.create_with_size(working_dir, command, size)
    }

    pub fn kill(&self) -> Result<()> {
        self.inner.kill()
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        self.inner.get_pane_pid()
    }

    pub fn attach(&self) -> Result<()> {
        self.inner.attach()
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        self.inner.capture_pane(lines)
    }
}

/// Build the argument list for tmux new-session command (terminal sessions).
/// Extracted for testability.
fn build_terminal_create_args(
    session_name: &str,
    working_dir: &str,
    command: Option<&str>,
    size: Option<(u16, u16)>,
) -> Vec<String> {
    let mut args = vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        session_name.to_string(),
        "-c".to_string(),
        working_dir.to_string(),
    ];

    if let Some((width, height)) = size {
        args.push("-x".to_string());
        args.push(width.to_string());
        args.push("-y".to_string());
        args.push(height.to_string());
    }

    if let Some(cmd) = command {
        args.push(cmd.to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::test_helpers::TmuxTestSession;
    use crate::tmux::{Session, SESSION_PREFIX};

    #[test]
    fn test_terminal_session_generate_name() {
        let name = TerminalSession::generate_name("abc123def456", "My Project");
        assert!(name.starts_with(TERMINAL_PREFIX));
        assert!(name.contains("My_Project"));
        assert!(name.contains("abc123de"));
    }

    #[test]
    fn test_container_terminal_session_generate_name() {
        let name = ContainerTerminalSession::generate_name("abc123def456", "My Project");
        assert!(name.starts_with(CONTAINER_TERMINAL_PREFIX));
        assert!(name.contains("My_Project"));
        assert!(name.contains("abc123de"));
    }

    #[test]
    fn test_terminal_session_name_differs_from_agent_session() {
        let agent_name = Session::generate_name("abc123def456", "My Project");
        let terminal_name = TerminalSession::generate_name("abc123def456", "My Project");
        assert_ne!(agent_name, terminal_name);
        assert!(agent_name.starts_with(SESSION_PREFIX));
        assert!(terminal_name.starts_with(TERMINAL_PREFIX));
    }

    #[test]
    fn test_container_terminal_name_differs_from_host_terminal() {
        let host_name = TerminalSession::generate_name("abc123def456", "My Project");
        let container_name = ContainerTerminalSession::generate_name("abc123def456", "My Project");
        assert_ne!(host_name, container_name);
        assert!(host_name.starts_with(TERMINAL_PREFIX));
        assert!(container_name.starts_with(CONTAINER_TERMINAL_PREFIX));
    }

    #[test]
    fn test_build_terminal_create_args_without_size() {
        let args = build_terminal_create_args("test_terminal", "/tmp/work", None, None);
        assert_eq!(
            args,
            vec![
                "new-session",
                "-d",
                "-s",
                "test_terminal",
                "-c",
                "/tmp/work"
            ]
        );
        assert!(!args.contains(&"-x".to_string()));
        assert!(!args.contains(&"-y".to_string()));
    }

    #[test]
    fn test_build_terminal_create_args_with_size() {
        let args = build_terminal_create_args("test_terminal", "/tmp/work", None, Some((100, 30)));
        assert!(args.contains(&"-x".to_string()));
        assert!(args.contains(&"100".to_string()));
        assert!(args.contains(&"-y".to_string()));
        assert!(args.contains(&"30".to_string()));

        // Verify order: -x should come before width, -y before height
        let x_idx = args.iter().position(|a| a == "-x").unwrap();
        let y_idx = args.iter().position(|a| a == "-y").unwrap();
        assert_eq!(args[x_idx + 1], "100");
        assert_eq!(args[y_idx + 1], "30");
    }

    #[test]
    fn test_build_terminal_create_args_with_command() {
        let args = build_terminal_create_args(
            "test_terminal",
            "/tmp/work",
            Some("docker exec -it container /bin/bash"),
            None,
        );
        assert_eq!(args.last().unwrap(), "docker exec -it container /bin/bash");
    }

    #[test]
    fn test_build_terminal_create_args_with_size_and_command() {
        let args = build_terminal_create_args(
            "test_terminal",
            "/tmp/work",
            Some("docker exec -it container /bin/bash"),
            Some((80, 24)),
        );

        // Size args should be present
        assert!(args.contains(&"-x".to_string()));
        assert!(args.contains(&"80".to_string()));
        assert!(args.contains(&"-y".to_string()));
        assert!(args.contains(&"24".to_string()));

        // Command should be last
        assert_eq!(args.last().unwrap(), "docker exec -it container /bin/bash");
    }

    fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_after_command_exits() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_terminal_dead");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            inner: PairedTerminal {
                name: session_name.clone(),
                kind: TerminalKind::Host,
            },
        };

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 1",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(1500));

        assert!(
            session.is_pane_dead(),
            "Terminal session pane should be dead after command exits"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_on_running_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_terminal_alive");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            inner: PairedTerminal {
                name: session_name.clone(),
                kind: TerminalKind::Host,
            },
        };

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            !session.is_pane_dead(),
            "Terminal session pane should be alive while command running"
        );
    }
}

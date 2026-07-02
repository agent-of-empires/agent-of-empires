//! Paired terminal sessions — host (`TerminalSession`) and sandbox (`ContainerTerminalSession`).
//!
//! The two session types have nearly identical lifecycles, so the
//! implementation lives in [`PairedTerminal`] and the public types are thin
//! wrappers that fix the tmux name prefix and the log-message label.

use anyhow::{bail, Result};

use super::utils::{
    append_clipboard_passthrough_args, append_default_shell_args, append_mouse_on_args,
    append_pane_base_index_args, append_remain_on_exit_args, append_window_size_args, is_pane_dead,
    sanitize_session_name,
};
use super::{
    refresh_session_cache, session_exists_from_cache, CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::config::should_apply_tmux_clipboard;
use crate::session::environment::{login_shell_command, user_shell};

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

/// Pure computation of the host-terminal `-e` env pairs and the effective
/// pane command, split out so the #2608 poisoning fix is unit-testable
/// without spawning tmux. `shell` is `Some` only for host terminals; `home`
/// and `path` are the resolved (possibly empty) host values, and empty
/// entries are dropped. When no command is supplied, a host terminal
/// defaults to the resolved login shell.
fn host_pane_inputs(
    shell: Option<&str>,
    command: Option<&str>,
    home: &str,
    path: &str,
) -> (Vec<(String, String)>, Option<String>) {
    let Some(shell) = shell else {
        return (Vec::new(), command.map(str::to_string));
    };
    let mut pairs = Vec::new();
    if !home.is_empty() {
        pairs.push(("HOME".to_string(), home.to_string()));
    }
    if !path.is_empty() {
        pairs.push(("PATH".to_string(), path.to_string()));
    }
    pairs.push(("SHELL".to_string(), shell.to_string()));
    let cmd = command
        .map(str::to_string)
        .or_else(|| Some(login_shell_command(shell)));
    (pairs, cmd)
}

/// Shared implementation of the paired-terminal lifecycle. Not exposed; the
/// public [`TerminalSession`] and [`ContainerTerminalSession`] wrap one of
/// these with a fixed [`TerminalKind`].
struct PairedTerminal {
    name: String,
    kind: TerminalKind,
}

impl PairedTerminal {
    fn generate_name(kind: TerminalKind, id: &str, title: &str, index: u32) -> String {
        let safe_title = sanitize_session_name(title);
        let base = format!("{}{}_{}", kind.prefix(), safe_title, truncate_id(id, 8));
        // Index 0 keeps the historical name verbatim, so existing tmux
        // sessions, URLs, and the native TUI (which only ever uses index 0)
        // are untouched. Additional web terminals get a `_t{N}` suffix.
        if index == 0 {
            base
        } else {
            format!("{base}_t{index}")
        }
    }

    fn new(kind: TerminalKind, id: &str, title: &str, index: u32) -> Self {
        Self {
            name: Self::generate_name(kind, id, title, index),
            kind,
        }
    }

    fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        crate::tmux::tmux_command()
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

        // Host terminals pin the pane's HOME/SHELL/PATH and launch the user's
        // login shell explicitly, so they never inherit a stale value from the
        // shared tmux server's frozen base environment: a dev build started
        // with a sandboxed HOME/SHELL can win the race to start the shared
        // server and poison `default-shell` + base env for every session,
        // including release ones (#2608). Container terminals are excluded;
        // their HOME/shell belong to the container, not the host.
        let host_shell = matches!(self.kind, TerminalKind::Host).then(user_shell);
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::env::var("PATH").unwrap_or_default();
        let (env_pairs, effective_cmd) =
            host_pane_inputs(host_shell.as_deref(), command, &home, &path);
        let env_refs: Vec<(&str, &str)> = env_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let mut args = super::session::build_create_args(
            &self.name,
            working_dir,
            &env_refs,
            effective_cmd.as_deref(),
            size,
        );
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if let Some(shell) = &host_shell {
            append_default_shell_args(&mut args, &self.name, shell);
        }
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

        let output = crate::tmux::tmux_command().args(&args).output()?;

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
            let status = crate::tmux::tmux_command()
                .args(["switch-client", "-t", &self.name])
                .status()?;

            if !status.success() {
                let status = crate::tmux::tmux_command()
                    .args(["attach-session", "-t", &self.name])
                    .status()?;

                if !status.success() {
                    bail!("Failed to attach to {}", self.kind.label());
                }
            }
        } else {
            let status = crate::tmux::tmux_command()
                .args(["attach-session", "-t", &self.name])
                .status()?;

            if !status.success() {
                bail!("Failed to attach to {}", self.kind.label());
            }
        }

        Ok(())
    }

    fn capture_pane(&self, lines: usize) -> Result<String> {
        // Shared with the agent session / web live view paths: same
        // `^.0` targeting and trailing-blank preservation semantics.
        super::Session::from_name(&self.name).capture_pane(lines)
    }
}

pub struct TerminalSession {
    inner: PairedTerminal,
}

impl TerminalSession {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Self::new_indexed(id, title, 0)
    }

    pub fn new_indexed(id: &str, title: &str, index: u32) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Host, id, title, index),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        Self::generate_name_indexed(id, title, 0)
    }

    pub fn generate_name_indexed(id: &str, title: &str, index: u32) -> String {
        PairedTerminal::generate_name(TerminalKind::Host, id, title, index)
    }

    pub fn name(&self) -> &str {
        &self.inner.name
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
        Self::new_indexed(id, title, 0)
    }

    pub fn new_indexed(id: &str, title: &str, index: u32) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Container, id, title, index),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        Self::generate_name_indexed(id, title, 0)
    }

    pub fn generate_name_indexed(id: &str, title: &str, index: u32) -> String {
        PairedTerminal::generate_name(TerminalKind::Container, id, title, index)
    }

    pub fn name(&self) -> &str {
        &self.inner.name
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

/// Kill every paired terminal tmux session (host and container, any index)
/// belonging to `id`. The single-index `kill` methods only target one
/// deterministic name; this scans the live session list so the multi-terminal
/// web tabs (`_t{N}` suffixes) and any title-change orphans are all reaped on
/// session teardown. Mirrors [`crate::tmux::kill_all_tool_sessions_for_id`].
pub fn kill_all_terminals_for_id(id: &str) {
    let needle = format!("_{}", truncate_id(id, 8));

    let output = crate::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if !line.starts_with(TERMINAL_PREFIX)
                    && !line.starts_with(CONTAINER_TERMINAL_PREFIX)
                {
                    continue;
                }
                // The id segment is at the end for index 0, or immediately
                // before the `_t{N}` suffix for additional terminals.
                let Some(pos) = line.rfind(&needle) else {
                    continue;
                };
                let after = &line[pos + needle.len()..];
                if !after.is_empty() && !after.starts_with("_t") {
                    continue;
                }
                if let Some(pid) = process::get_pane_pid(line) {
                    process::kill_process_tree(pid);
                }
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", line])
                    .output();
            }
        }
    }

    refresh_session_cache();
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
    fn test_terminal_index_zero_matches_legacy_name() {
        // Index 0 must be byte-identical to the historical single-terminal
        // name so existing tmux sessions, URLs, and the TUI keep working.
        let legacy = TerminalSession::generate_name("abc123def456", "My Project");
        let indexed_zero = TerminalSession::generate_name_indexed("abc123def456", "My Project", 0);
        assert_eq!(legacy, indexed_zero);

        let legacy_c = ContainerTerminalSession::generate_name("abc123def456", "My Project");
        let indexed_zero_c =
            ContainerTerminalSession::generate_name_indexed("abc123def456", "My Project", 0);
        assert_eq!(legacy_c, indexed_zero_c);
    }

    #[test]
    fn test_terminal_index_nonzero_suffixed_and_distinct() {
        let zero = TerminalSession::generate_name_indexed("abc123def456", "My Project", 0);
        let one = TerminalSession::generate_name_indexed("abc123def456", "My Project", 1);
        let two = TerminalSession::generate_name_indexed("abc123def456", "My Project", 2);
        assert_ne!(zero, one);
        assert_ne!(one, two);
        assert!(one.ends_with("_t1"));
        assert!(two.ends_with("_t2"));
        assert!(one.starts_with(&zero));
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
    fn test_host_pane_inputs_injects_env_and_login_shell() {
        // Regression for #2608: a host terminal with no explicit command must
        // pin HOME/PATH/SHELL and launch the user's login shell, so the pane
        // no longer inherits the poisoned shared-server env / default-shell.
        let (env, cmd) = host_pane_inputs(Some("/bin/zsh"), None, "/Users/me", "/usr/bin:/bin");
        assert_eq!(
            env,
            vec![
                ("HOME".to_string(), "/Users/me".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("SHELL".to_string(), "/bin/zsh".to_string()),
            ]
        );
        assert_eq!(cmd.as_deref(), Some("'/bin/zsh' -l"));
    }

    #[test]
    fn test_host_pane_inputs_keeps_explicit_command() {
        let (env, cmd) = host_pane_inputs(Some("/bin/zsh"), Some("htop"), "/Users/me", "/bin");
        // Env is still pinned, but an explicit command is not overridden.
        assert!(env.contains(&("SHELL".to_string(), "/bin/zsh".to_string())));
        assert_eq!(cmd.as_deref(), Some("htop"));
    }

    #[test]
    fn test_host_pane_inputs_drops_empty_home_path() {
        let (env, _) = host_pane_inputs(Some("/bin/bash"), None, "", "");
        assert_eq!(env, vec![("SHELL".to_string(), "/bin/bash".to_string())]);
    }

    #[test]
    fn test_container_pane_inputs_unchanged() {
        // Container terminals (shell = None) get no host env and keep their
        // command verbatim; their HOME/shell belong to the container.
        let (env, cmd) = host_pane_inputs(None, Some("bash -lc enter"), "/Users/me", "/bin");
        assert!(env.is_empty());
        assert_eq!(cmd.as_deref(), Some("bash -lc enter"));

        let (env_none, cmd_none) = host_pane_inputs(None, None, "/Users/me", "/bin");
        assert!(env_none.is_empty());
        assert!(cmd_none.is_none());
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
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

        let output = crate::tmux::tmux_command()
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

        let output = crate::tmux::tmux_command()
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

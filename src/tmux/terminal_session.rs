//! Paired terminal sessions — host (`TerminalSession`) and sandbox (`ContainerTerminalSession`).
//!
//! The two session types have nearly identical lifecycles, so the
//! implementation lives in [`PairedTerminal`] and the public types are thin
//! wrappers that fix the tmux name prefix and the log-message label.

use anyhow::{bail, Result};

use super::utils::{
    append_default_shell_args, append_pane_base_index_args, append_remain_on_exit_args,
    append_tmux_setting_args, append_window_size_args, is_pane_dead, sanitize_session_name,
};
use super::{refresh_session_cache, CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX};
use crate::cli::truncate_id;
use crate::process;
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

    /// The tail every paired-terminal name for this session id and `index`
    /// ends with. Index 0 keeps the bare `_<id8>`; later web terminals append
    /// `_t<N>`, which keeps the indices from resolving onto each other (a
    /// `_t10` name does not end with `_t1`, since the match is anchored at the
    /// very end of the name).
    fn name_suffix(id: &str, index: u32) -> String {
        let id_suffix = format!("_{}", truncate_id(id, 8));
        if index == 0 {
            id_suffix
        } else {
            format!("{id_suffix}_t{index}")
        }
    }

    /// The paired terminal to ACT on: the title-derived name normally, or the
    /// live terminal carrying this id's tail when the stored title has moved out
    /// from under it. Without this a retitled session's terminal view would
    /// spawn a fresh shell under the new name and orphan the pane the user was
    /// working in, exactly as the agent pane did before #3157.
    fn resolve_name(kind: TerminalKind, id: &str, title: &str, index: u32) -> String {
        let derived = Self::generate_name(kind, id, title, index);
        let suffix = Self::name_suffix(id, index);
        crate::tmux::live_session_name(
            &derived,
            &crate::tmux::NameShape {
                prefix: kind.prefix(),
                suffix: &suffix,
                excluded_prefixes: &[],
            },
        )
    }

    fn new(kind: TerminalKind, id: &str, title: &str, index: u32) -> Self {
        Self {
            name: Self::resolve_name(kind, id, title, index),
            kind,
        }
    }

    fn exists(&self) -> bool {
        crate::tmux::session_exists(&self.name)
    }

    fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
    ) -> Result<()> {
        if self.exists() {
            return Ok(());
        }
        let config = crate::tmux::tmux_option_config(profile);

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
        let (pinned_pairs, effective_cmd) =
            host_pane_inputs(host_shell.as_deref(), command, &home, &path);
        // Host terminals also forward the inherited host env (DISPLAY, XDG_*,
        // DBUS, ... plus every other var under `session.inherit_host_environment`)
        // so a browser opened from the pane reaches the user's desktop;
        // container terminals keep the container's own env (#3075, #3262).
        //
        // Ordered inherited-then-pinned, and the pinned pairs must stay last: a
        // later `-e` wins, and under passthrough the inherited layer carries
        // HOME/PATH too, which would otherwise undo the deliberate pinning
        // above and reintroduce #2608.
        let mut env_pairs = if matches!(self.kind, TerminalKind::Host) {
            crate::session::environment::inherited_host_env(profile)
        } else {
            Vec::new()
        };
        env_pairs.extend(pinned_pairs);
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
        append_window_size_args(&mut args, &self.name);
        if let Some(shell) = &host_shell {
            append_default_shell_args(&mut args, &self.name, shell);
        }
        append_tmux_setting_args(&mut args, &self.name, &config);

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

    fn capture_window_composited(&self, lines: usize) -> Result<String> {
        if !self.exists() {
            return Ok(String::new());
        }
        super::Session::from_name(&self.name).capture_window_composited(lines)
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

    /// The name of the paired terminal to ACT on: the title-derived name
    /// normally, or the live terminal carrying this session id's tail when the
    /// stored title has moved out from under it. Callers wanting the session's
    /// CURRENT name want this; [`Self::generate_name`] stays a pure derivation
    /// for computing the name to rename TO.
    pub fn resolve_name(id: &str, title: &str) -> String {
        Self::resolve_name_indexed(id, title, 0)
    }

    /// [`Self::resolve_name`] for the web dashboard's additional terminal tabs.
    pub fn resolve_name_indexed(id: &str, title: &str, index: u32) -> String {
        PairedTerminal::resolve_name(TerminalKind::Host, id, title, index)
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

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
        profile: &str,
    ) -> Result<()> {
        self.inner
            .create_with_size(working_dir, command, size, profile)
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

    /// Preview capture with the window's other panes composited in; see
    /// [`super::Session::capture_window_composited`].
    pub fn capture_window_composited(&self, lines: usize) -> Result<String> {
        self.inner.capture_window_composited(lines)
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

    /// The name of the paired terminal to ACT on: the title-derived name
    /// normally, or the live terminal carrying this session id's tail when the
    /// stored title has moved out from under it. Callers wanting the session's
    /// CURRENT name want this; [`Self::generate_name`] stays a pure derivation
    /// for computing the name to rename TO.
    pub fn resolve_name(id: &str, title: &str) -> String {
        Self::resolve_name_indexed(id, title, 0)
    }

    /// [`Self::resolve_name`] for the web dashboard's additional terminal tabs.
    pub fn resolve_name_indexed(id: &str, title: &str, index: u32) -> String {
        PairedTerminal::resolve_name(TerminalKind::Container, id, title, index)
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
        profile: &str,
    ) -> Result<()> {
        self.inner
            .create_with_size(working_dir, command, size, profile)
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

    /// Preview capture with the window's other panes composited in; see
    /// [`super::Session::capture_window_composited`].
    pub fn capture_window_composited(&self, lines: usize) -> Result<String> {
        self.inner.capture_window_composited(lines)
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

    /// A session id long enough that `truncate_id(.., 8)` truncates, so the
    /// tests exercise the real `_<id8>` tail.
    const ID: &str = "abc12345deadbeef";

    #[test]
    fn name_suffix_keeps_terminal_indices_from_resolving_onto_each_other() {
        // The tail is what resolution matches on, so index isolation lives here.
        // `_t10` must not satisfy index 1's tail, which holds only because the
        // match is anchored at the very end of the name.
        let zero = PairedTerminal::name_suffix(ID, 0);
        let one = PairedTerminal::name_suffix(ID, 1);
        let ten = PairedTerminal::name_suffix(ID, 10);
        assert_eq!(zero, "_abc12345");
        assert_eq!(one, "_abc12345_t1");
        assert_eq!(ten, "_abc12345_t10");
        let named =
            |idx: u32| PairedTerminal::generate_name(TerminalKind::Host, ID, "Vikings", idx);
        assert!(!named(1).ends_with(&zero), "index 1 must not match index 0");
        assert!(!named(0).ends_with(&one), "index 0 must not match index 1");
        assert!(
            !named(10).ends_with(&one),
            "index 10 must not match index 1"
        );
        assert!(named(10).ends_with(&ten));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_name_adopts_a_retitled_terminal_and_ignores_other_indices() {
        // #3157 for the paired terminal: the title moved, the terminal kept the
        // name it was created under. Reopening the terminal view must reattach
        // to the running shell instead of spawning a fresh one beside it.
        let guard = crate::tmux::SessionCacheGuard::capture();
        let stale = TerminalSession::generate_name(ID, "Vikings");
        let stale_t1 = TerminalSession::generate_name_indexed(ID, "Vikings", 1);
        guard.force_present(&[stale.as_str(), stale_t1.as_str()]);

        assert_eq!(TerminalSession::resolve_name(ID, "Refactor billing"), stale);
        // Through the constructor too: that is the path every call site takes,
        // so `create` adopts the running shell instead of spawning beside it.
        assert_eq!(
            TerminalSession::new(ID, "Refactor billing")
                .expect("terminal")
                .name(),
            stale
        );
        assert_eq!(
            TerminalSession::resolve_name_indexed(ID, "Refactor billing", 1),
            stale_t1,
            "each terminal tab resolves onto its own index, not another's"
        );
        // Index 2 was never created, so it keeps the name it will be spawned
        // under rather than adopting tab 0 or 1.
        assert_eq!(
            TerminalSession::resolve_name_indexed(ID, "Refactor billing", 2),
            TerminalSession::generate_name_indexed(ID, "Refactor billing", 2)
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_name_keeps_host_and_container_terminals_apart() {
        // Only the container terminal is live. The host terminal must NOT adopt
        // it: they are different panes with different shells.
        let guard = crate::tmux::SessionCacheGuard::capture();
        let container = ContainerTerminalSession::generate_name(ID, "Vikings");
        guard.force_present(&[container.as_str()]);

        assert_eq!(
            ContainerTerminalSession::resolve_name(ID, "Refactor billing"),
            container
        );
        assert_eq!(
            ContainerTerminalSession::new(ID, "Refactor billing")
                .expect("container terminal")
                .name(),
            container
        );
        assert_eq!(
            TerminalSession::resolve_name(ID, "Refactor billing"),
            TerminalSession::generate_name(ID, "Refactor billing"),
            "a live container terminal is not the host terminal"
        );
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

    /// Drive the real `create_with_size` for a host terminal and assert the
    /// desktop/session env is forwarded, so a revert of the host-terminal
    /// `inherited_host_env()` layer is caught (#3075). Uses an `XDG_`
    /// sentinel so the forwarding rule matches it without colliding with real
    /// config or another test's assertions.
    #[test]
    #[serial_test::serial]
    fn test_host_terminal_forwards_desktop_env() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let key = "XDG_AOE_TERM_ENV_TEST_3075";
        let original = std::env::var(key).ok();
        std::env::set_var(key, "host-sentinel");

        let guard = TmuxTestSession::new("aoe_test_term_host_fwd");
        let session = PairedTerminal {
            name: guard.name().to_string(),
            kind: TerminalKind::Host,
        };
        let created = session.create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");

        let shown = crate::tmux::tmux_command()
            .args(["show-environment", "-t", guard.name(), key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());

        match original {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }

        created.expect("create host terminal");
        assert_eq!(
            shown.as_deref(),
            Some("XDG_AOE_TERM_ENV_TEST_3075=host-sentinel"),
            "a host terminal must carry the forwarded desktop/session env (#3075)"
        );
    }

    /// Container terminals keep the container's own env; the host desktop env
    /// (DISPLAY, XDG_*, SSH_AUTH_SOCK, ...) must NOT leak into them. Drive the
    /// real `create_with_size` for a container terminal (a plain command, no
    /// Docker) and assert the sentinel is absent, locking the `matches!(Host)`
    /// exclusion so dropping the guard is caught (#3075).
    #[test]
    #[serial_test::serial]
    fn test_container_terminal_excludes_desktop_env() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let key = "XDG_AOE_TERM_ENV_TEST_3075_CTR";
        let original = std::env::var(key).ok();
        std::env::set_var(key, "must-not-leak");

        let guard = TmuxTestSession::new("aoe_test_term_ctr_excl");
        let session = PairedTerminal {
            name: guard.name().to_string(),
            kind: TerminalKind::Container,
        };
        let created = session.create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");

        // `show-environment` exits non-zero and prints nothing to stdout for an
        // unknown variable, so a forwarded var yields the `KEY=VALUE` line and
        // an excluded one yields an empty string.
        let shown = crate::tmux::tmux_command()
            .args(["show-environment", "-t", guard.name(), key])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());

        match original {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }

        created.expect("create container terminal");
        assert_eq!(
            shown.as_deref(),
            Some(""),
            "a container terminal must NOT inherit the host desktop/session env (#3075)"
        );
    }
}

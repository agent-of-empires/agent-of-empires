//! Paired terminal sessions: host ([`TerminalSession`]) and sandbox
//! ([`ContainerTerminalSession`]), one generic implementation.

use anyhow::{bail, Result};

use super::utils::{
    append_session_setup_args, attach_client, create_session_tolerating_duplicate, is_pane_dead,
    kill_session_tree, kill_sessions_matching, sanitize_session_name,
};
use super::{SessionKind, CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX};
use crate::cli::truncate_id;
use crate::process;
use crate::session::environment::{login_shell_command, user_shell};

pub type TerminalSession = PairedTerminal<false>;
/// Uses its own `aoe_cterm_` prefix so host and container terminals coexist.
pub type ContainerTerminalSession = PairedTerminal<true>;

pub struct PairedTerminal<const CONTAINER: bool> {
    name: String,
}

/// Host terminal `-e` pairs and pane command. `shell` is `Some` only for host
/// terminals; empty `home`/`path` are dropped, and the command defaults to the
/// login shell.
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

/// tmux's `default-shell` rejects bare names, and [`user_shell`] yields `bash`
/// when `$SHELL` is unset (a daemon). `None` when the shell cannot be found.
fn absolute_shell(shell: &str) -> Option<String> {
    absolute_shell_in(shell, std::env::var_os("PATH").as_deref())
}

fn absolute_shell_in(shell: &str, paths: Option<&std::ffi::OsStr>) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    which::which_in(shell, paths, cwd)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

impl<const CONTAINER: bool> PairedTerminal<CONTAINER> {
    const PREFIX: &'static str = if CONTAINER {
        CONTAINER_TERMINAL_PREFIX
    } else {
        TERMINAL_PREFIX
    };
    const KIND: SessionKind = if CONTAINER {
        SessionKind::ContainerTerminal
    } else {
        SessionKind::Terminal
    };
    const LABEL: &'static str = if CONTAINER {
        "container terminal session"
    } else {
        "terminal session"
    };

    pub fn new(id: &str, title: &str) -> Result<Self> {
        Self::new_indexed(id, title, 0)
    }

    pub fn new_indexed(id: &str, title: &str, index: u32) -> Result<Self> {
        Ok(Self {
            name: Self::resolve_name_indexed(id, title, index),
        })
    }

    /// The name to act on: the live terminal carrying this id's tail when the
    /// title has moved, else the derived name.
    pub fn resolve_name(id: &str, title: &str) -> String {
        Self::resolve_name_indexed(id, title, 0)
    }

    pub fn resolve_name_indexed(id: &str, title: &str, index: u32) -> String {
        Self::resolve(id, title, index, false)
    }

    /// Snapshot-only [`Self::resolve_name`] for render paths.
    pub fn resolve_name_for_display(id: &str, title: &str) -> String {
        Self::resolve(id, title, 0, true)
    }

    fn resolve(id: &str, title: &str, index: u32, display_only: bool) -> String {
        let derived = Self::generate_name_indexed(id, title, index);
        let suffix = Self::name_suffix(id, index);
        let shape = crate::tmux::NameShape {
            prefix: Self::PREFIX,
            suffix: &suffix,
            kind: Self::KIND,
        };
        if display_only {
            crate::tmux::session_name_for_display(&derived, &shape)
        } else {
            crate::tmux::live_session_name(&derived, &shape)
        }
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        Self::generate_name_indexed(id, title, 0)
    }

    pub fn generate_name_indexed(id: &str, title: &str, index: u32) -> String {
        format!(
            "{}{}{}",
            Self::PREFIX,
            sanitize_session_name(title),
            Self::name_suffix(id, index)
        )
    }

    /// `_<id8>` for index 0 (the native TUI's terminal), `_<id8>_t<N>` for extra
    /// web terminals. Matched at the very end, so `_t10` never resolves as `_t1`.
    fn name_suffix(id: &str, index: u32) -> String {
        let id_suffix = format!("_{}", truncate_id(id, 8));
        if index == 0 {
            id_suffix
        } else {
            format!("{id_suffix}_t{index}")
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        crate::tmux::session_exists(&self.name)
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn create_with_size(
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

        // Host terminals pin HOME/SHELL/PATH and launch the login shell
        // explicitly: the shared tmux server's base env may have been poisoned
        // by a sandboxed dev build. `default-shell` is pinned only when the
        // shell resolves to an executable, so it can never fail `new-session`.
        let host_shell = (!CONTAINER).then(user_shell);
        let default_shell = host_shell.as_deref().and_then(absolute_shell);
        let shell_for_pane = default_shell.as_deref().or(host_shell.as_deref());
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::env::var("PATH").unwrap_or_default();
        let (pinned_pairs, effective_cmd) = host_pane_inputs(shell_for_pane, command, &home, &path);
        // Inherited host env first: a later `-e` wins, so the pinned pairs stay last.
        let mut env_pairs = if CONTAINER {
            Vec::new()
        } else {
            crate::session::environment::inherited_host_env(profile)
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
        append_session_setup_args(
            &mut args,
            &self.name,
            &config,
            default_shell.as_deref(),
            Self::KIND,
        );
        create_session_tolerating_duplicate(&args, |stderr| {
            format!("Failed to create {}: {}", Self::LABEL, stderr)
        })
    }

    pub fn kill(&self) -> Result<()> {
        kill_session_tree(&self.name)
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }

    pub fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("{} does not exist: {}", Self::LABEL, self.name);
        }
        if attach_client(&self.name)?.is_some() {
            bail!("Failed to attach to {}", Self::LABEL);
        }
        Ok(())
    }
}

/// Kill every host and container terminal (any index) for `id`, including
/// title-change orphans.
pub fn kill_all_terminals_for_id(id: &str) {
    let needle = format!("_{}", truncate_id(id, 8));
    kill_sessions_matching(|name| {
        if !name.starts_with(TERMINAL_PREFIX) && !name.starts_with(CONTAINER_TERMINAL_PREFIX) {
            return false;
        }
        // The id ends the name, or precedes a `_t{N}` suffix.
        name.rfind(&needle).is_some_and(|pos| {
            let after = &name[pos + needle.len()..];
            after.is_empty() || after.starts_with("_t")
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::test_helpers::require_tmux;
    use crate::tmux::test_helpers::TmuxTestSession;
    use crate::tmux::{Session, SESSION_PREFIX};

    #[test]
    fn test_terminal_session_generate_name() {
        assert_eq!(
            TerminalSession::generate_name("abc123def456", "My Project"),
            format!("{TERMINAL_PREFIX}My_Project_abc123de")
        );
        assert_eq!(
            ContainerTerminalSession::generate_name("abc123def456", "My Project"),
            format!("{CONTAINER_TERMINAL_PREFIX}My_Project_abc123de")
        );
    }

    const ID: &str = "abc12345deadbeef";

    #[test]
    fn name_suffix_keeps_terminal_indices_from_resolving_onto_each_other() {
        let zero = TerminalSession::name_suffix(ID, 0);
        let one = TerminalSession::name_suffix(ID, 1);
        let ten = TerminalSession::name_suffix(ID, 10);
        assert_eq!(zero, "_abc12345");
        assert_eq!(one, "_abc12345_t1");
        assert_eq!(ten, "_abc12345_t10");
        let named = |idx: u32| TerminalSession::generate_name_indexed(ID, "Vikings", idx);
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
        let guard = crate::tmux::SessionCacheGuard::capture();
        let stale = TerminalSession::generate_name(ID, "Vikings");
        let stale_t1 = TerminalSession::generate_name_indexed(ID, "Vikings", 1);
        guard.force_present(&[stale.as_str(), stale_t1.as_str()]);

        assert_eq!(TerminalSession::resolve_name(ID, "Refactor billing"), stale);
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
        assert_eq!(
            TerminalSession::resolve_name_indexed(ID, "Refactor billing", 2),
            TerminalSession::generate_name_indexed(ID, "Refactor billing", 2)
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_name_keeps_host_and_container_terminals_apart() {
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
        assert!(env.contains(&("SHELL".to_string(), "/bin/zsh".to_string())));
        assert_eq!(cmd.as_deref(), Some("htop"));
    }

    #[test]
    fn test_host_pane_inputs_drops_empty_home_path() {
        let (env, _) = host_pane_inputs(Some("/bin/bash"), None, "", "");
        assert_eq!(env, vec![("SHELL".to_string(), "/bin/bash".to_string())]);
    }

    #[test]
    fn absolute_shell_resolves_bare_name_and_rejects_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("aoe-test-shell");
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let paths = Some(dir.path().as_os_str());
        let resolved = absolute_shell_in("aoe-test-shell", paths).expect("shim must resolve");
        assert_eq!(std::path::Path::new(&resolved), bin);
        assert_eq!(absolute_shell_in("aoe-not-a-real-shell-xyzzy", paths), None);
    }

    #[test]
    fn test_container_pane_inputs_unchanged() {
        let (env, cmd) = host_pane_inputs(None, Some("bash -lc enter"), "/Users/me", "/bin");
        assert!(env.is_empty());
        assert_eq!(cmd.as_deref(), Some("bash -lc enter"));

        let (env_none, cmd_none) = host_pane_inputs(None, None, "/Users/me", "/bin");
        assert!(env_none.is_empty());
        assert!(cmd_none.is_none());
    }
    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_after_command_exits() {
        use crate::tmux::test_helpers::{only_pane_id, wait_for_pane_dead};

        let _env = crate::session::test_support::EnvGuard::read_lock();
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_terminal_dead");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            name: session_name.clone(),
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

        wait_for_pane_dead(&only_pane_id(&session_name));

        assert!(
            session.is_pane_dead(),
            "Terminal session pane should be dead after command exits"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_on_running_session() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        require_tmux!();

        let guard = TmuxTestSession::new("aoe_test_terminal_alive");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            name: session_name.clone(),
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
                "sleep",
                "30",
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

        let pane_id = crate::tmux::test_helpers::only_pane_id(&session_name);
        crate::tmux::test_helpers::wait_for_pane_command(&pane_id, "sleep");
        assert_eq!(
            crate::tmux::test_helpers::pane_field(&pane_id, "#{pane_dead}"),
            "0"
        );

        assert!(
            !session.is_pane_dead(),
            "Terminal session pane should be alive while command running"
        );
    }

    #[test]
    #[serial_test::serial]
    fn only_host_terminals_forward_desktop_env() {
        require_tmux!();

        let key = "XDG_AOE_TERM_ENV_TEST_3075";
        let _env = crate::session::test_support::EnvGuard::set(&[
            (key, "host-sentinel"),
            ("SHELL", "/bin/sh"),
        ]);

        let show = |name: &str| {
            crate::tmux::tmux_command()
                .args(["show-environment", "-t", name, key])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        };

        let host = TmuxTestSession::new("aoe_test_term_host_fwd");
        let created = TerminalSession {
            name: host.name().to_string(),
        }
        .create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");
        let shown = show(host.name());
        created.expect("create host terminal");
        assert_eq!(
            shown.as_deref(),
            Some("XDG_AOE_TERM_ENV_TEST_3075=host-sentinel")
        );

        let container = TmuxTestSession::new("aoe_test_term_ctr_excl");
        let created = ContainerTerminalSession {
            name: container.name().to_string(),
        }
        .create_with_size("/tmp", Some("sleep 5"), Some((80, 24)), "default");
        let shown = show(container.name());
        created.expect("create container terminal");
        assert_eq!(shown.as_deref(), Some(""));
    }
}

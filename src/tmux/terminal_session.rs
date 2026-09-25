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
    fn terminal_names_carry_their_kind_prefix_and_index_suffix() {
        let (id, title) = ("abc123def456", "My Project");
        let host = format!("{TERMINAL_PREFIX}My_Project_abc123de");
        let container = format!("{CONTAINER_TERMINAL_PREFIX}My_Project_abc123de");
        // Index zero keeps the legacy unsuffixed name.
        for (name, expected) in [
            (TerminalSession::generate_name(id, title), host.clone()),
            (
                TerminalSession::generate_name_indexed(id, title, 0),
                host.clone(),
            ),
            (
                TerminalSession::generate_name_indexed(id, title, 1),
                format!("{host}_t1"),
            ),
            (
                TerminalSession::generate_name_indexed(id, title, 2),
                format!("{host}_t2"),
            ),
            (
                ContainerTerminalSession::generate_name(id, title),
                container.clone(),
            ),
            (
                ContainerTerminalSession::generate_name_indexed(id, title, 0),
                container,
            ),
        ] {
            assert_eq!(name, expected);
        }
        let agent = Session::generate_name(id, title);
        assert!(agent.starts_with(SESSION_PREFIX) && agent != host);
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
    fn host_pane_inputs_inject_env_and_a_login_shell_only_on_the_host() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // (shell, command, home, path) -> (env, command)
        let cases = [
            (
                (Some("/bin/zsh"), None, "/Users/me", "/usr/bin:/bin"),
                (
                    env(&[
                        ("HOME", "/Users/me"),
                        ("PATH", "/usr/bin:/bin"),
                        ("SHELL", "/bin/zsh"),
                    ]),
                    Some("'/bin/zsh' -l"),
                ),
            ),
            (
                (Some("/bin/zsh"), Some("htop"), "/Users/me", "/bin"),
                (
                    env(&[
                        ("HOME", "/Users/me"),
                        ("PATH", "/bin"),
                        ("SHELL", "/bin/zsh"),
                    ]),
                    Some("htop"),
                ),
            ),
            (
                (Some("/bin/bash"), None, "", ""),
                (env(&[("SHELL", "/bin/bash")]), Some("'/bin/bash' -l")),
            ),
            // A container pane (no host shell) is passed through untouched.
            (
                (None, Some("bash -lc enter"), "/Users/me", "/bin"),
                (vec![], Some("bash -lc enter")),
            ),
            ((None, None, "/Users/me", "/bin"), (vec![], None)),
        ];
        for ((shell, command, home, path), (want_env, want_command)) in cases {
            let (got_env, got_command) = host_pane_inputs(shell, command, home, path);
            assert_eq!(got_env, want_env, "{shell:?} {command:?}");
            assert_eq!(
                got_command.as_deref(),
                want_command,
                "{shell:?} {command:?}"
            );
        }
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

//! Agent hooks that write session status (`running`/`waiting`/`idle`) and
//! session ids to per-instance sidecar files. Events per agent are declared in
//! `crate::agents`; pane reconciliation covers gaps hooks cannot see.

mod codex;
mod command;
mod config_io;
mod dir_guard;
mod hermes;
mod json_settings;
mod kimi;
mod kiro;
mod settl;
mod status_file;
mod targets;
mod trust;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};

pub use codex::uninstall_codex_hooks;
pub(crate) use codex::{
    install_codex_hooks_with_preserved_state, install_codex_json_hooks, restore_codex_hooks_state,
    snapshot_codex_hooks_state,
};
pub(crate) use command::HOOK_STATUS_BASE_IN_CONTAINER;
#[cfg(test)]
pub(crate) use command::{hook_command, hook_command_session_id, status_command_for_event};
pub(crate) use config_io::with_config_lock_policy;
pub use config_io::SymlinkPolicy;
pub(crate) use dir_guard::{
    ensure_instance_dir_path, hook_base_path, unlink_session_id_via_guard,
    write_session_id_via_guard,
};
pub use hermes::{install_hermes_hooks_with_events, uninstall_hermes_hooks};
pub use json_settings::{
    install_cursor_hooks_with_events, install_hooks, uninstall_cursor_hooks, uninstall_hooks,
};
pub use kimi::{install_kimi_hooks_with_events, uninstall_kimi_hooks};
pub use kiro::{
    install_kiro_hooks_with_events, resolve_kiro_agent_file, set_kiro_default_agent_if_builtin,
    uninstall_kiro_hooks, KIRO_HOOKS_AGENT_FILE,
};
pub use settl::{install_settl_hooks_with_events, uninstall_settl_hooks};
pub use status_file::{
    cleanup_hook_status_dir, hook_status_dir, read_hook_session_id, read_hook_session_id_any_age,
    read_hook_session_path, read_hook_status, read_hook_status_age, read_hook_urgent,
    session_id_sidecar_exists,
};
pub(crate) use targets::{
    has_aoe_marker, iter_hook_targets, iter_hook_targets_in, HookTarget, HookTargetKind,
};
pub use trust::{
    disable_gemini_folder_trust, trust_claude_project, trust_codex_project, trust_gemini_project,
    trust_host_project,
};

/// Where the agent's settings live, which decides the base directory baked into
/// commands and how the session id is extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInstallTarget {
    Host,
    Sandbox,
}

/// `<CODEX_HOME>/config.toml`, else `<home>/.codex/config.toml`.
pub(crate) fn codex_config_path_in(home: &Path, host_env: &[String]) -> PathBuf {
    codex_home_in(home, host_env).join("config.toml")
}

/// `<CODEX_HOME>/hooks.json`, else `<home>/.codex/hooks.json`.
pub(crate) fn codex_hooks_json_path_in(home: &Path, host_env: &[String]) -> PathBuf {
    codex_home_in(home, host_env).join("hooks.json")
}

fn codex_home_in(home: &Path, host_env: &[String]) -> PathBuf {
    resolve_config_dir_override("CODEX_HOME", host_env)
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

/// The agent's settings file: `<config dir>/<basename>` when its config-dir
/// variable is set (it replaces the whole `~/.claude`-style dir), else
/// `<home>/<settings_rel_path>`.
pub(crate) fn agent_settings_path_in(
    home: &Path,
    hook_cfg: &crate::agents::AgentHookConfig,
    host_env: &[String],
) -> PathBuf {
    let override_dir = hook_cfg
        .config_dir_env_var
        .and_then(|var| resolve_config_dir_override(var, host_env));
    let file = Path::new(hook_cfg.settings_rel_path).file_name();
    match (override_dir, file) {
        (Some(dir), Some(file)) => PathBuf::from(dir).join(file),
        _ => home.join(hook_cfg.settings_rel_path),
    }
}

/// A config-dir variable from the session's host environment, else AoE's own
/// (which the agent inherits). Empty values count as unset.
pub(crate) fn resolve_config_dir_override(var: &str, host_env: &[String]) -> Option<String> {
    crate::session::environment::resolve_host_environment_value(host_env, var)
        .or_else(|| std::env::var(var).ok())
        .filter(|v| !v.is_empty())
}

/// Remove AoE hooks from every known agent config and the hook status base.
/// Called by `aoe uninstall`.
pub fn uninstall_all_hooks() {
    for target in iter_hook_targets() {
        let result = match target.kind {
            HookTargetKind::JsonSettings | HookTargetKind::CodexJson => {
                uninstall_hooks(&target.path)
            }
            HookTargetKind::CodexToml => uninstall_codex_hooks(&target.path),
            HookTargetKind::Sidecar(sidecar) => (sidecar.uninstall)(&target.path),
        };
        match result {
            Ok(true) => println!("Removed AoE hooks from {}", target.path.display()),
            Ok(false) => {}
            Err(e) => tracing::warn!(target: "hooks.uninstall",
                "Failed to remove {} hooks from {}: {}",
                target.agent_name,
                target.path.display(),
                e
            ),
        }
    }

    let base = dir_guard::hook_base_path();
    if base.exists() {
        if let Err(e) = std::fs::remove_dir_all(&base) {
            tracing::warn!(target: "hooks.uninstall", "Failed to remove {}: {}", base.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{agent_events, assert_not_rewritten};
    use crate::session::test_support::EnvGuard;
    use tempfile::TempDir;

    #[test]
    #[serial_test::serial(shell_env)]
    fn config_paths_follow_env_overrides() {
        let home = Path::new("/home/me");
        let claude = crate::agents::get_agent("claude")
            .unwrap()
            .hook_config
            .as_ref()
            .unwrap();
        let cases: [(Option<&str>, &[&str], &str, &str); 6] = [
            (
                None,
                &[],
                "/home/me/.claude/settings.json",
                "/home/me/.codex/hooks.json",
            ),
            (
                None,
                &["CLAUDE_CONFIG_DIR=/work/claude", "CODEX_HOME=/work/codex"],
                "/work/claude/settings.json",
                "/work/codex/hooks.json",
            ),
            // The session's host env wins over AoE's own env.
            (
                Some("/proc/env"),
                &["CLAUDE_CONFIG_DIR=/host/env", "CODEX_HOME=/host/env"],
                "/host/env/settings.json",
                "/host/env/hooks.json",
            ),
            // AoE's env is inherited by the agent, so hooks follow it.
            (
                Some("/proc/env"),
                &[],
                "/proc/env/settings.json",
                "/proc/env/hooks.json",
            ),
            // Empty values never resolve to a bare relative file.
            (
                None,
                &["CLAUDE_CONFIG_DIR=", "CODEX_HOME="],
                "/home/me/.claude/settings.json",
                "/home/me/.codex/hooks.json",
            ),
            (
                Some(""),
                &[],
                "/home/me/.claude/settings.json",
                "/home/me/.codex/hooks.json",
            ),
        ];
        for (process_env, host_env, claude_want, codex_want) in cases {
            let _guard = match process_env {
                Some(value) => {
                    EnvGuard::set(&[("CLAUDE_CONFIG_DIR", value), ("CODEX_HOME", value)])
                }
                None => EnvGuard::unset(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]),
            };
            let host_env: Vec<String> = host_env.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                agent_settings_path_in(home, claude, &host_env),
                Path::new(claude_want)
            );
            assert_eq!(
                codex_hooks_json_path_in(home, &host_env),
                Path::new(codex_want)
            );
        }
    }

    /// Reinstalling identical hooks must leave every file untouched.
    #[test]
    fn reinstall_does_not_rewrite_config() {
        type Install = fn(&Path) -> anyhow::Result<()>;
        let cases: [(&str, Install); 7] = [
            ("settings.json", |p| {
                install_hooks(p, agent_events("claude", &[]), HookInstallTarget::Host)
            }),
            ("config.toml", |p| {
                codex::install_codex_hooks(p, agent_events("codex", &[]))
            }),
            ("config.toml", |p| {
                let state = snapshot_codex_hooks_state(p)?;
                install_codex_hooks_with_preserved_state(
                    p,
                    agent_events("codex", &[]),
                    state,
                    HookInstallTarget::Host,
                )
            }),
            ("config.toml", |p| {
                install_settl_hooks_with_events(
                    p,
                    HookInstallTarget::Host,
                    &agent_events("settl", &[]),
                )
            }),
            ("config.toml", |p| {
                install_kimi_hooks_with_events(
                    p,
                    HookInstallTarget::Host,
                    &agent_events("kimi", &[]),
                )
            }),
            ("config.yaml", |p| {
                install_hermes_hooks_with_events(
                    p,
                    HookInstallTarget::Host,
                    &agent_events("hermes", &[]),
                )
            }),
            ("aoe-hooks.json", |p| {
                install_kiro_hooks_with_events(
                    p,
                    HookInstallTarget::Host,
                    &agent_events("kiro", &[]),
                )
            }),
        ];
        for (file, install) in cases {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join(file);
            install(&path).unwrap();
            let allowlist = tmp.path().join("shell-hooks-allowlist.json");
            assert_not_rewritten(&path, || {
                if allowlist.exists() {
                    assert_not_rewritten(&allowlist, || install(&path).unwrap());
                } else {
                    install(&path).unwrap();
                }
            });
        }
    }
}

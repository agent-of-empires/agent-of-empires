//! What installing an agent's status hooks writes, and the one-time
//! acknowledgement that gates writing it.
//!
//! The TUI renders the disclosure in its approval dialog and the daemon serves
//! it, so a client creating a session on another machine can show that
//! machine's paths rather than its own.

use crate::agents::AgentDef;
use crate::session::config::SessionConfig;

/// One hook event and the effect it has.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookCommand {
    pub event: String,
    /// Such as `writes "waiting"`.
    pub writes: String,
}

/// Every file installing an agent's hooks would touch, resolved for one
/// machine, tool and profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookDisclosure {
    pub settings_paths: Vec<String>,
    pub hook_commands: Vec<HookCommand>,
    /// The single command every installed hook runs.
    pub status_write_command: String,
    /// Codex refuses the hooks until the user trusts them in `/hooks`.
    pub needs_codex_trust_note: bool,
}

/// A host launch refused because this machine has never acknowledged what
/// installing the agent's hooks writes.
#[derive(Debug, thiserror::Error)]
#[error("agent hook paths have not been acknowledged; approve them in the AoE TUI on this machine before launching this host session")]
pub struct AgentHooksNotAcknowledged;

/// Whether this machine has acknowledged agent hook installation.
pub fn agent_hooks_acknowledged() -> bool {
    crate::session::config::load_config()
        .ok()
        .flatten()
        .is_some_and(|config| config.app_state.has_acknowledged_agent_hooks)
}

/// The agent whose hooks `tool_name` installs, following `agent_detect_as`.
/// `None` when the tool installs no hooks at all.
pub fn hook_install_agent(
    tool_name: &str,
    session_config: &SessionConfig,
) -> Option<&'static AgentDef> {
    crate::agents::get_agent(tool_name)
        .or_else(|| {
            session_config
                .agent_detect_as
                .get(tool_name)
                .and_then(|detect_as| crate::agents::get_agent(detect_as))
        })
        .filter(|agent| agent.hook_config.is_some() || agent.sidecar_hooks.is_some())
}

/// What installing `agent_name`'s hooks for `tool_name` under `profile` writes
/// on this machine. Empty for an agent that installs none.
pub fn hook_disclosure(tool_name: &str, agent_name: &str, profile: Option<&str>) -> HookDisclosure {
    let mut disclosure = HookDisclosure {
        // The euid in the path matches the runtime path baked into the hook
        // command and is already exposed via `id -u` and `ps`; a placeholder
        // would misstate what is installed.
        status_write_command: format!(
            "printf {{status}} > {}/$AOE_INSTANCE_ID/status",
            crate::hooks::hook_base_path().display()
        ),
        ..HookDisclosure::default()
    };
    let Some(agent) = crate::agents::get_agent(agent_name) else {
        return disclosure;
    };
    let profile_config =
        profile.map(crate::session::config::profile_config::resolve_config_or_warn);
    let host_environment = profile_config
        .as_ref()
        .map(|config| config.environment.as_slice())
        .unwrap_or_default();
    let home =
        crate::session::environment::resolve_host_environment_value(host_environment, "HOME")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("~"));
    let default_config = SessionConfig::default();
    let session_config = profile_config
        .as_ref()
        .map(|config| &config.session)
        .unwrap_or(&default_config);

    if let Some(hook_cfg) = &agent.hook_config {
        disclosure.needs_codex_trust_note = hook_cfg.format == crate::agents::HookFormat::CodexJson;
        disclosure.settings_paths.push(
            crate::session::generic_host_config_path_for(
                tool_name,
                hook_cfg,
                &home,
                session_config,
                host_environment,
            )
            .to_string_lossy()
            .into_owned(),
        );
        for event in hook_cfg.events {
            disclosure.hook_commands.push(HookCommand {
                event: event.name.to_string(),
                writes: match event.status {
                    Some(status) => format!("writes \"{}\"", status),
                    None => "session lifecycle".to_string(),
                },
            });
        }
    } else if let Some(sidecar) = &agent.sidecar_hooks {
        disclosure.settings_paths.push(
            crate::session::sidecar_host_config_path_for(
                tool_name,
                agent,
                sidecar,
                &home,
                session_config,
                host_environment,
            )
            .to_string_lossy()
            .into_owned(),
        );
        for event in sidecar.events {
            disclosure.hook_commands.push(HookCommand {
                event: event.name.to_string(),
                writes: format!("writes \"{}\"", event.status),
            });
        }
    }
    disclosure
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;
    use tempfile::TempDir;

    /// The production pairing: resolve the agent behind the tool, then disclose
    /// what installing its hooks writes.
    fn hook_disclosure_for_tool(tool_name: &str, profile: Option<&str>) -> HookDisclosure {
        let session_config = profile
            .map(crate::session::config::profile_config::resolve_config_or_warn)
            .map(|config| config.session)
            .unwrap_or_default();
        let agent_name =
            hook_install_agent(tool_name, &session_config).map_or(tool_name, |agent| agent.name);
        hook_disclosure(tool_name, agent_name, profile)
    }

    fn paths(disclosure: &HookDisclosure) -> Vec<String> {
        disclosure.settings_paths.clone()
    }

    #[test]
    #[serial_test::serial]
    fn a_tool_without_hooks_discloses_nothing() {
        let disclosure = hook_disclosure_for_tool("bash", None);
        assert!(disclosure.settings_paths.is_empty());
        assert!(disclosure.hook_commands.is_empty());
    }

    #[test]
    fn the_hook_agent_follows_detect_as_only_for_unknown_tools() {
        // (tool, detect_as entry, expected resolved agent)
        for (tool, detect_as, expected) in [
            (
                "wrapped-codex",
                Some(("wrapped-codex", "codex")),
                Some("codex"),
            ),
            // A built-in agent wins over a detect_as alias, and opencode
            // installs no hooks.
            ("opencode", Some(("opencode", "codex")), None),
            (
                "wrapped-agent",
                Some(("wrapped-agent", "missing-agent")),
                None,
            ),
            ("bash", None, None),
        ] {
            let mut config = SessionConfig::default();
            if let Some((from, to)) = detect_as {
                config
                    .agent_detect_as
                    .insert(from.to_string(), to.to_string());
            }
            assert_eq!(
                hook_install_agent(tool, &config).map(|agent| agent.name),
                expected,
                "{tool}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn claude_discloses_its_settings_file_and_status_events() {
        let _overrides = EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let disclosure = hook_disclosure_for_tool("claude", None);
        assert!(
            paths(&disclosure)[0].ends_with(".claude/settings.json"),
            "{:?}",
            disclosure.settings_paths
        );
        let events: Vec<&str> = disclosure
            .hook_commands
            .iter()
            .map(|command| command.event.as_str())
            .collect();
        assert!(events.contains(&"Stop"), "{events:?}");
        assert!(!disclosure.needs_codex_trust_note);
    }

    #[test]
    #[serial_test::serial]
    fn codex_discloses_its_hooks_file_and_asks_for_the_trust_note() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);
        let disclosure = hook_disclosure_for_tool("codex", None);
        assert_eq!(
            paths(&disclosure),
            vec![tmp.path().join("hooks.json").to_string_lossy()]
        );
        assert!(disclosure.needs_codex_trust_note);
    }

    #[test]
    #[serial_test::serial]
    fn a_profile_home_override_moves_every_disclosed_path() {
        let temp = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let profile_home = temp.path().join("profile-home");
        let _environment = EnvGuard::set(&[("AOE_TEST_HOOK_HOME", profile_home.as_os_str())]);
        let _overrides = EnvGuard::unset(&["CODEX_HOME", "CLAUDE_CONFIG_DIR"]);
        let profile_dir = crate::session::get_profile_dir("profile-home").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "environment = [\"HOME=$AOE_TEST_HOOK_HOME\"]\n",
        )
        .unwrap();

        assert_eq!(
            paths(&hook_disclosure_for_tool("claude", Some("profile-home"))),
            vec![profile_home.join(".claude/settings.json").to_string_lossy()]
        );
        assert_eq!(
            paths(&hook_disclosure_for_tool("codex", Some("profile-home"))),
            vec![profile_home.join(".codex/hooks.json").to_string_lossy()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_declared_agent_config_root_wins_over_the_default_path() {
        let temp = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let claude_root = temp.path().join("claude-custom");
        let codex_root = temp.path().join("codex-custom");
        let profile_dir = crate::session::get_profile_dir("declared-hook-roots").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!(
                "[session.agent_config_dir]\nclaude = \"{}\"\ncodex = \"{}\"\n",
                claude_root.display(),
                codex_root.display()
            ),
        )
        .unwrap();

        assert_eq!(
            paths(&hook_disclosure_for_tool(
                "claude",
                Some("declared-hook-roots")
            )),
            vec![claude_root.join("settings.json").to_string_lossy()]
        );
        assert_eq!(
            paths(&hook_disclosure_for_tool(
                "codex",
                Some("declared-hook-roots")
            )),
            vec![codex_root.join("hooks.json").to_string_lossy()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_aliased_tool_discloses_the_detected_agent_inherited_path() {
        let temp = TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let custom = temp.path().join("cursor-custom");
        let _cursor = EnvGuard::set(&[("CURSOR_CONFIG_DIR", custom.as_os_str())]);
        let profile_dir = crate::session::get_app_dir().unwrap().join("profiles/work");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "environment = [\"CURSOR_CONFIG_DIR\"]\n\n[session.agent_detect_as]\ncorp-cursor = \"cursor\"\n",
        )
        .unwrap();

        let disclosure = hook_disclosure_for_tool("corp-cursor", Some("work"));
        assert_eq!(
            paths(&disclosure),
            vec![custom.join("hooks.json").to_string_lossy()]
        );
        assert!(!disclosure.hook_commands.is_empty());
    }
}

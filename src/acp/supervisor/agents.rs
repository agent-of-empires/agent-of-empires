//! Agent resolution: which registry or custom spec a session launches.

use std::collections::HashMap;
use std::path::Path;

use tracing::warn;

use super::{AgentCommandOverride, BroadcastSink, Supervisor, SupervisorError};
use crate::acp::agent_policy::AgentPolicy;
use crate::acp::agent_registry::{AgentRegistry, AgentSpec};
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;

impl<S: BroadcastSink> Supervisor<S> {
    pub async fn resolve_agent(&self, name: &str) -> Result<AgentSpec, SupervisorError> {
        self.registry
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| SupervisorError::UnknownAgent(name.into()))
    }

    /// Resolve `name` against the registry, then the session's custom
    /// `agent_acp_cmd`, then an `agent_detect_as` base. The bool is true for a
    /// registry spec.
    pub async fn resolve_agent_spec(
        &self,
        name: &str,
        config: &crate::session::config::SessionConfig,
        policy: &AgentPolicy,
    ) -> Result<(AgentSpec, bool), SupervisorError> {
        if !policy.allows(name) {
            return Err(SupervisorError::AgentNotAllowed(name.into()));
        }
        if let Some(spec) = self.registry.lock().await.get(name).cloned() {
            return Ok((spec, true));
        }
        if let Some(cmd) = config.agent_acp_cmd.get(name) {
            let spec =
                AgentSpec::from_acp_cmd(name, cmd).map_err(SupervisorError::InvalidAgentCommand)?;
            return Ok((spec, false));
        }
        if let Some(base) = crate::acp::inherited_acp_base(name, &config.agent_detect_as) {
            if let Some(spec) = self.registry.lock().await.get(&base).cloned() {
                return Ok((spec, true));
            }
        }
        Err(SupervisorError::UnknownAgent(name.into()))
    }

    /// Pick the agent name to spawn for an instance's tool.
    pub async fn pick_agent_for_tool(
        &self,
        tool: &str,
        explicit_override: Option<&str>,
        profile: &str,
        project_path: &Path,
    ) -> String {
        if let Some(name) = explicit_override.filter(|name| !name.is_empty()) {
            return name.to_string();
        }
        if self.registry_has_agent(tool).await
            || self
                .custom_agent_has_acp_cmd(tool, profile, project_path)
                .await
        {
            return tool.to_string();
        }
        if let Some(base) = self
            .custom_agent_inherited_base(tool, profile, project_path)
            .await
        {
            return base;
        }
        if tool == "claude" {
            return "claude".into();
        }
        with_resolved_config(profile, project_path, |cfg| {
            cfg.acp.resolved_default_agent().to_string()
        })
        .await
        .unwrap_or_else(|| crate::session::config::DEFAULT_ACP_AGENT.to_string())
    }

    /// The registry-backed base `tool` inherits via `agent_detect_as`.
    pub async fn custom_agent_inherited_base(
        &self,
        tool: &str,
        profile: &str,
        project_path: &Path,
    ) -> Option<String> {
        let tool = tool.to_string();
        with_resolved_config(profile, project_path, move |cfg| {
            crate::acp::inherited_acp_base(&tool, &cfg.session.agent_detect_as)
        })
        .await
        .flatten()
    }

    /// True iff `tool` declares a valid `agent_acp_cmd` in its resolved config.
    pub async fn custom_agent_has_acp_cmd(
        &self,
        tool: &str,
        profile: &str,
        project_path: &Path,
    ) -> bool {
        let tool = tool.to_string();
        with_resolved_config(profile, project_path, move |cfg| {
            cfg.session
                .agent_acp_cmd
                .get(&tool)
                .is_some_and(|cmd| AgentSpec::from_acp_cmd(&tool, cmd).is_ok())
        })
        .await
        .unwrap_or(false)
    }

    pub async fn registry_snapshot(&self) -> AgentRegistry {
        self.registry.lock().await.clone()
    }

    pub async fn registry_has_agent(&self, name: &str) -> bool {
        self.registry.lock().await.get(name).is_some()
    }

    /// True iff `name` is a valid structured-view switch target for this profile and project.
    pub async fn agent_is_valid_switch_target(
        &self,
        name: &str,
        profile: &str,
        project_path: &Path,
    ) -> bool {
        self.registry_has_agent(name).await
            || self
                .custom_agent_has_acp_cmd(name, profile, project_path)
                .await
            || self
                .custom_agent_inherited_base(name, profile, project_path)
                .await
                .is_some()
    }
}

/// Resolve the profile + repo config off the async runtime; `None` if the task panics.
async fn with_resolved_config<T: Send + 'static>(
    profile: &str,
    project_path: &Path,
    f: impl FnOnce(crate::session::config::Config) -> T + Send + 'static,
) -> Option<T> {
    let profile = profile.to_string();
    let project_path = project_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        f(resolve_config_with_repo_or_warn(&profile, &project_path))
    })
    .await
    .ok()
}

/// True when `command` names `binary`, by file name so absolute paths match.
fn command_matches_binary(command: &str, binary: &str) -> bool {
    command == binary
        || Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name == binary)
}

/// Overlay an instance command override onto a registry spec, keeping the
/// registry args after the override's own. Skipped unless the spec is the
/// logical tool's own built-in adapter binary.
pub(crate) fn apply_agent_command_override(
    selected_agent: &str,
    spec_from_registry: bool,
    ovr: &AgentCommandOverride,
    spec: &mut AgentSpec,
) -> Result<(), SupervisorError> {
    if !spec_from_registry || selected_agent != ovr.logical_tool {
        return Ok(());
    }
    let Some(agent_def) = crate::agents::get_agent(&ovr.logical_tool) else {
        return Ok(());
    };
    if !command_matches_binary(&spec.command, agent_def.binary) {
        return Ok(());
    }
    let mut argv = shell_words::split(&ovr.command)
        .map_err(|e| SupervisorError::InvalidAgentCommand(format!("{e}")))?;
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Ok(());
    }
    spec.command = argv.remove(0);
    argv.append(&mut spec.args);
    spec.args = argv;
    Ok(())
}

/// The `(wrapper, base)` pair when an `agent_detect_as` wrapper launches its
/// base adapter instead of its own binary.
pub(super) fn wrapper_substitution_for(
    registry: &AgentRegistry,
    tool: &str,
    agent: &str,
    spec_from_registry: bool,
    agent_detect_as: &HashMap<String, String>,
) -> Option<(String, String)> {
    if !spec_from_registry || agent_detect_as.is_empty() {
        return None;
    }
    let inherited = |name: &str| crate::acp::inherited_acp_base(name, agent_detect_as);
    let tool_base = inherited(tool);
    let substituted = if agent == tool {
        // A built-in running itself is never a substitution.
        registry.get(tool).is_none().then_some((tool, tool_base))
    } else if tool_base.as_deref() == Some(agent) && registry.get(tool).is_none() {
        Some((tool, tool_base))
    } else if registry.get(agent).is_none() {
        let agent_base = inherited(agent);
        agent_base.is_some().then_some((agent, agent_base))
    } else {
        None
    };
    // A base of None is a terminal-only base, which resolution never substitutes.
    let (wrapper, Some(base)) = substituted? else {
        return None;
    };
    Some((wrapper.to_string(), base))
}

pub(super) fn log_wrapper_substitution(session_id: &str, tool: &str, wrapper: &str, base: &str) {
    warn!(
        target: "acp.supervisor",
        session = %session_id,
        tool = %tool,
        wrapper = %wrapper,
        base = %base,
        "agent_detect_as resolved this wrapper to its base for structured view; the wrapper binary will not be executed, so account, gateway, or env overrides it sets do not apply; set [session.agent_acp_cmd] to run the wrapper itself"
    );
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn spec(command: &str, args: &[&str]) -> AgentSpec {
        AgentSpec {
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            description: "test".into(),
            env_allowlist: None,
        }
    }

    #[test]
    fn command_override_applies_only_to_the_tools_own_registry_binary() {
        // (agent, from registry, spec command, spec args, override tool, override, want command, want args)
        type Case<'a> = (
            &'a str,
            bool,
            &'a str,
            &'a [&'a str],
            &'a str,
            &'a str,
            &'a str,
            &'a [&'a str],
        );
        let cases: [Case; 5] = [
            (
                "opencode",
                true,
                "opencode",
                &["acp"],
                "opencode",
                "opencode-plannotator",
                "opencode-plannotator",
                &["acp"],
            ),
            (
                "opencode",
                true,
                "opencode",
                &["acp"],
                "opencode",
                "opencode-plannotator --profile plan",
                "opencode-plannotator",
                &["--profile", "plan", "acp"],
            ),
            (
                "opencode",
                false,
                "opencode",
                &["acp"],
                "opencode",
                "opencode-plannotator",
                "opencode",
                &["acp"],
            ),
            (
                "claude",
                true,
                "claude-agent-acp",
                &[],
                "claude",
                "claude-wrapper",
                "claude-agent-acp",
                &[],
            ),
            (
                "aoe-agent",
                true,
                "aoe-agent",
                &[],
                "opencode",
                "opencode-plannotator",
                "aoe-agent",
                &[],
            ),
        ];
        for (agent, from_registry, command, args, tool, ovr, want_command, want_args) in cases {
            let mut s = spec(command, args);
            let ovr = AgentCommandOverride {
                logical_tool: tool.into(),
                command: ovr.into(),
            };
            apply_agent_command_override(agent, from_registry, &ovr, &mut s).unwrap();
            assert_eq!(
                (s.command.as_str(), s.args.as_slice()),
                (
                    want_command,
                    want_args
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .as_slice()
                ),
                "{agent} {ovr:?}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_agent_spec_honors_the_agent_allowlist() {
        let sup = Supervisor::new(VecSink::new());
        let mut cfg = crate::session::config::SessionConfig::default();
        cfg.agent_acp_cmd
            .insert("oc-superpowers".into(), "ocp run sp acp".into());
        cfg.agent_detect_as
            .insert("lenovo-claude".into(), "claude".into());

        #[derive(Debug)]
        enum Want {
            Registry,
            Custom,
            NotAllowed,
            Unknown,
        }
        let cases = [
            (false, &[][..], "claude", Want::Registry),
            (false, &[][..], "oc-superpowers", Want::Custom),
            (false, &[][..], "lenovo-claude", Want::Registry),
            (false, &[][..], "no-such-agent", Want::Unknown),
            (true, &["claude"][..], "claude", Want::Registry),
            (true, &["claude"][..], "codex", Want::NotAllowed),
            (
                true,
                &["oc-superpowers"][..],
                "oc-superpowers",
                Want::Custom,
            ),
            (true, &["claude"][..], "oc-superpowers", Want::NotAllowed),
            (
                true,
                &["lenovo-claude"][..],
                "lenovo-claude",
                Want::Registry,
            ),
            (true, &["claude"][..], "lenovo-claude", Want::NotAllowed),
            (true, &["claude"][..], "no-such-agent", Want::NotAllowed),
            (true, &[][..], "claude", Want::NotAllowed),
        ];
        for (restrict, allowed, name, want) in cases {
            let policy = AgentPolicy::for_test(restrict, allowed);
            let got = sup.resolve_agent_spec(name, &cfg, &policy).await;
            let label = format!("restrict={restrict} allowed={allowed:?} name={name:?}");
            match want {
                Want::Registry => {
                    let (_, from_registry) = got.unwrap_or_else(|e| panic!("{label}: {e}"));
                    assert!(from_registry, "{label}: expected a registry spec");
                }
                Want::Custom => {
                    let (spec, from_registry) = got.unwrap_or_else(|e| panic!("{label}: {e}"));
                    assert!(!from_registry, "{label}: expected a custom spec");
                    assert_eq!(spec.command, "ocp", "{label}");
                }
                Want::NotAllowed => assert!(
                    matches!(got, Err(SupervisorError::AgentNotAllowed(ref n)) if n == name),
                    "{label}: expected AgentNotAllowed, got {got:?}"
                ),
                Want::Unknown => assert!(
                    matches!(got, Err(SupervisorError::UnknownAgent(_))),
                    "{label}: expected UnknownAgent, got {got:?}"
                ),
            }
        }
    }

    #[test]
    fn wrapper_substitution_is_reported_only_when_a_wrapper_runs_its_base() {
        let registry = AgentRegistry::with_defaults();
        let map = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let detect_as = map(&[
            ("claude-personal", "claude"),
            ("codex-personal", "codex"),
            ("kimi-personal", "kimi"),
        ]);
        // (tool, agent, expected wrapper and base)
        for (tool, agent, want) in [
            ("claude-personal", "claude", ("claude-personal", "claude")),
            (
                "codex-personal",
                "codex-personal",
                ("codex-personal", "codex"),
            ),
            ("plain-tool", "kimi-personal", ("kimi-personal", "kimi")),
        ] {
            let got = wrapper_substitution_for(&registry, tool, agent, true, &detect_as);
            assert_eq!(
                got.as_ref().map(|(w, b)| (w.as_str(), b.as_str())),
                Some(want),
                "{tool:?} -> {agent:?}"
            );
        }

        let mapped_builtin = map(&[("claude", "codex")]);
        let silent = [
            ("claude", "claude", true, detect_as.clone()),
            ("claude", "claude", true, mapped_builtin.clone()),
            ("claude", "claude", true, map(&[("claude", "claude")])),
            ("claude", "codex", true, mapped_builtin),
            ("plain-tool", "codex", true, map(&[("codex", "claude")])),
            (
                "claude-personal",
                "claude-personal",
                true,
                map(&[("claude-personal", "cursor")]),
            ),
            ("claude-personal", "codex", true, detect_as.clone()),
            ("claude-personal", "claude-personal", false, detect_as),
            ("plain-tool", "aoe-agent", true, HashMap::new()),
        ];
        for (tool, agent, from_registry, detect_as) in silent {
            assert_eq!(
                wrapper_substitution_for(&registry, tool, agent, from_registry, &detect_as),
                None,
                "{tool} -> {agent} ({from_registry}, {detect_as:?})"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unregistered_tool_falls_back_to_the_configured_default_agent() {
        let (_home, tmp) = isolate_home();
        let cfg_path = crate::session::get_app_dir().unwrap().join("config.toml");
        let sup = Supervisor::new(VecSink::new());

        // (configured acp.default_agent, tool, expected agent)
        let cases = [
            (None, "plain-tool", "claude-code"),
            (None, "claude", "claude"),
            (Some("codex"), "opencode", "opencode"),
            (Some("codex"), "plain-tool", "codex"),
            (Some("codex"), "claude", "claude"),
            (Some("   "), "plain-tool", "claude-code"),
        ];
        for (configured, tool, expected) in cases {
            let body = configured
                .map(|agent| format!("[acp]\ndefault_agent = \"{agent}\"\n"))
                .unwrap_or_default();
            std::fs::write(&cfg_path, body).unwrap();
            let got = sup.pick_agent_for_tool(tool, None, "", tmp.path()).await;
            assert_eq!(got, expected, "default={configured:?} tool={tool}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_path_emits_the_wrapper_warning() {
        let (_home, tmp) = isolate_home();
        let logs = crate::session::test_support::LogCapture::start();
        let sup = Supervisor::new(VecSink::new());
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "\n[session.agent_detect_as]\nclaude-personal = \"claude\"\n",
        )
        .unwrap();

        let mut req = spawn_request("s-wire");
        req.agent = "claude-personal".into();
        req.tool = "claude-personal".into();
        req.cwd = tmp.path().join("does-not-exist");
        assert!(
            sup.spawn(req).await.is_err(),
            "launch into a missing working directory must fail"
        );
        let logs = logs.contents();
        assert!(
            logs.contains("s-wire") && logs.contains("will not be executed"),
            "spawn path must emit the wrapper warning before the launch fails"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn agent_is_valid_switch_target_uses_the_profile_config() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let write = |path: std::path::PathBuf, body: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            r#"
[session.agent_acp_cmd]
cursor-acp-bridge = "agent acp"
broken = ""

[session.agent_detect_as]
lenovo-claude = "claude"
my-cursor = "cursor"
"#,
        );
        write(
            crate::session::get_profile_dir_path("cursor")
                .unwrap()
                .join("config.toml"),
            "[session.agent_acp_cmd]\nprofile-bridge = \"agent acp\"\n",
        );

        let cases = [
            ("claude", "", true),
            ("cursor-acp-bridge", "", true),
            ("unknown-agent", "", false),
            ("broken", "", false),
            ("lenovo-claude", "", true),
            // A terminal-only base has no ACP adapter.
            ("my-cursor", "", false),
            ("profile-bridge", "cursor", true),
            ("profile-bridge", "other", false),
        ];
        for (name, profile, expected) in cases {
            let got = sup
                .agent_is_valid_switch_target(name, profile, tmp.path())
                .await;
            assert_eq!(got, expected, "{name:?} in profile {profile:?}");
        }
    }
}

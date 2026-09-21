//! Named agent registry: maps an agent name (e.g. `claude`, `aoe-agent`) to a spawn command.

use super::install_hints::{env_allowlist_for, install_hint_for, AOE_AGENT_BINARY};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub command: String,
    pub args: Vec<String>,
    /// Shown in the settings TUI and `aoe acp agents`.
    pub description: String,
    /// Provider env vars forwarded on top of `ALWAYS_FORWARD_ENV` in `acp_client/spawn.rs`.
    pub env_allowlist: Option<Vec<String>>,
}

impl AgentSpec {
    /// Build a spec from a custom agent's `agent_acp_cmd` string.
    pub fn from_acp_cmd(name: &str, cmd: &str) -> Result<AgentSpec, String> {
        let argv = shell_words::split(cmd).map_err(|e| {
            format!("custom agent `{name}` has a malformed structured view command ({e})")
        })?;
        let mut argv = argv.into_iter();
        let command = argv
            .next()
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| format!("custom agent `{name}` has an empty structured view command"))?;
        Ok(AgentSpec {
            command,
            args: argv.collect(),
            description: format!("Custom ACP agent `{name}`"),
            env_allowlist: None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRegistry {
    pub agents: HashMap<String, AgentSpec>,
}

impl AgentRegistry {
    /// One entry per tool with a published ACP server, plus aoe's own `aoe-agent`.
    pub fn with_defaults() -> Self {
        let claude_install = install_hint_for("claude-agent-acp").unwrap_or("(see project docs)");
        let claude_description =
            format!("Anthropic Claude via the official ACP adapter ({claude_install})");
        // (name, command, args, description)
        let defaults: [(&str, &str, &[&str], &str); 11] = [
            ("claude", "claude-agent-acp", &[], &claude_description),
            // Legacy alias from older session records.
            (
                "claude-code",
                "claude-agent-acp",
                &[],
                "Alias for `claude` (legacy name)",
            ),
            (
                "opencode",
                "opencode",
                &["acp"],
                "OpenCode (SST), native ACP via `opencode acp`",
            ),
            (
                "gemini",
                "gemini",
                &["--acp"],
                "Google Gemini CLI, native ACP via `gemini --acp`",
            ),
            (
                "codex",
                "codex-acp",
                &[],
                "OpenAI Codex CLI via ACP adapter (npm i -g @agentclientprotocol/codex-acp@latest)",
            ),
            (
                "vibe",
                "vibe-acp",
                &[],
                "Mistral Vibe, native ACP via the bundled `vibe-acp` binary",
            ),
            (
                "pi",
                "pi-acp",
                &[],
                "Pi coding agent (`pi`) via the pi-acp adapter (npm i -g pi-acp)",
            ),
            (
                "omp",
                "omp",
                &["acp"],
                "Oh My Pi coding agent, native ACP via `omp acp`",
            ),
            (
                "kimi",
                "kimi",
                &["acp"],
                "Kimi Code (Moonshot AI), native ACP via `kimi acp`",
            ),
            (
                "prime-agent",
                "prime-agent",
                &["--mode", "acp"],
                "PrimeIntellect Prime Agent, native ACP via `prime-agent --mode acp`",
            ),
            // Installed into the app dir like the npm adapters.
            (
                "aoe-agent",
                AOE_AGENT_BINARY,
                &[],
                "aoe's bundled multi-provider agent (Vercel AI SDK)",
            ),
        ];
        let agents = defaults
            .into_iter()
            .map(|(name, command, args, description)| {
                let keys = env_allowlist_for(command);
                let spec = AgentSpec {
                    command: command.into(),
                    args: args.iter().map(|a| a.to_string()).collect(),
                    description: description.into(),
                    env_allowlist: (!keys.is_empty())
                        .then(|| keys.iter().map(|s| s.to_string()).collect()),
                };
                (name.to_string(), spec)
            })
            .collect();
        Self { agents }
    }

    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.agents.get(name)
    }

    /// Entries sorted by name.
    pub fn list(&self) -> Vec<(&String, &AgentSpec)> {
        let mut entries: Vec<_> = self.agents.iter().collect();
        entries.sort_by_key(|(n, _)| n.as_str());
        entries
    }
}

/// The built-in base `tool` inherits via `agent_detect_as`, when that base has an ACP adapter.
pub fn inherited_acp_base(tool: &str, agent_detect_as: &HashMap<String, String>) -> Option<String> {
    let base = agent_detect_as.get(tool)?;
    AgentRegistry::with_defaults()
        .get(base)
        .map(|_| base.clone())
}

/// The agent a structured-view session of `tool` spawns as: an explicit
/// override, the tool's own registry entry or custom command, an inherited
/// base, then (except for `claude`) the configured default agent.
pub fn pick_acp_agent_name(
    registry: &AgentRegistry,
    session: &crate::session::config::SessionConfig,
    acp: &crate::session::config::AcpConfig,
    tool: &str,
    explicit_override: Option<&str>,
) -> String {
    if let Some(name) = explicit_override.filter(|name| !name.is_empty()) {
        return name.to_string();
    }
    let custom_cmd = session
        .agent_acp_cmd
        .get(tool)
        .is_some_and(|cmd| AgentSpec::from_acp_cmd(tool, cmd).is_ok());
    if registry.get(tool).is_some() || custom_cmd {
        return tool.to_string();
    }
    if let Some(base) = inherited_acp_base(tool, &session.agent_detect_as) {
        return base;
    }
    if tool == "claude" {
        "claude".into()
    } else {
        acp.resolved_default_agent().to_string()
    }
}

/// The model pinned for the agent `tool` spawns as, if any.
pub fn pinned_model_for_tool(
    config: &crate::session::config::Config,
    tool: &str,
    explicit_override: Option<&str>,
) -> Option<String> {
    let agent = pick_acp_agent_name(
        &AgentRegistry::with_defaults(),
        &config.session,
        &config.acp,
        tool,
        explicit_override,
    );
    config.acp.pinned_model_for(&agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_acp_agent_name_resolves_the_agent_a_spawn_runs() {
        let registry = AgentRegistry::with_defaults();
        let session = crate::session::config::SessionConfig {
            agent_detect_as: [
                ("my-claude".to_string(), "claude".to_string()),
                ("my-cursor".to_string(), "cursor".to_string()),
                ("bad-sp".to_string(), "claude".to_string()),
            ]
            .into(),
            agent_acp_cmd: [
                ("oc-sp".to_string(), "ocp run sp acp".to_string()),
                ("bad-sp".to_string(), String::new()),
            ]
            .into(),
            ..Default::default()
        };
        let acp = crate::session::config::AcpConfig {
            default_agent: "opencode".into(),
            ..Default::default()
        };

        for (tool, explicit, want) in [
            ("claude", Some("gemini"), "gemini"),
            ("claude", Some(""), "claude"),
            ("opencode", None, "opencode"),
            ("oc-sp", None, "oc-sp"),
            ("my-claude", None, "claude"),
            ("my-cursor", None, "opencode"),
            ("claude", None, "claude"),
            ("unknown", None, "opencode"),
            ("bad-sp", None, "claude"),
        ] {
            assert_eq!(
                pick_acp_agent_name(&registry, &session, &acp, tool, explicit),
                want,
                "{tool} / {explicit:?}"
            );
        }

        let detect_as: HashMap<String, String> = [
            ("lenovo-claude", "claude"),
            ("work-codex", "codex"),
            // A terminal-only base has no adapter.
            ("my-cursor", "cursor"),
            // A base that is itself a custom name is not followed.
            ("chain", "lenovo-claude"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (tool, expected) in [
            ("lenovo-claude", Some("claude")),
            ("work-codex", Some("codex")),
            ("my-cursor", None),
            ("chain", None),
            ("unmapped", None),
        ] {
            assert_eq!(
                inherited_acp_base(tool, &detect_as).as_deref(),
                expected,
                "{tool}"
            );
        }
    }

    #[test]
    fn pinned_model_for_tool_reads_the_resolved_agents_pin() {
        let mut config = crate::session::config::Config::default();
        config
            .session
            .agent_detect_as
            .insert("my-claude".into(), "claude".into());
        config.acp.acp_defaults.insert(
            "claude".into(),
            crate::session::config::AcpAgentDefaults {
                model: Some("claude-pinned".into()),
                pin_model: true,
                ..Default::default()
            },
        );
        for (tool, explicit, want) in [
            ("my-claude", None, Some("claude-pinned")),
            ("claude", None, Some("claude-pinned")),
            ("claude", Some("gemini"), None),
            ("opencode", None, None),
        ] {
            assert_eq!(
                pinned_model_for_tool(&config, tool, explicit).as_deref(),
                want,
                "{tool}"
            );
        }
    }

    #[test]
    fn from_acp_cmd_splits_argv_and_rejects_bad_commands() {
        let spec = AgentSpec::from_acp_cmd("oc-sp", "ocp run sp acp").unwrap();
        assert_eq!(
            (spec.command.as_str(), spec.args.clone()),
            ("ocp", strings(&["run", "sp", "acp"]))
        );
        assert_eq!(spec.description, "Custom ACP agent `oc-sp`");
        assert!(spec.env_allowlist.is_none());

        let quoted = AgentSpec::from_acp_cmd("wrap", "sh -lc 'ocp run sp acp'").unwrap();
        assert_eq!(
            (quoted.command.as_str(), quoted.args),
            ("sh", strings(&["-lc", "ocp run sp acp"]))
        );
        for bad in ["", "   ", "ocp run \"unterminated"] {
            assert!(AgentSpec::from_acp_cmd("x", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn defaults_spawn_commands_and_sorted_listing() {
        let reg = AgentRegistry::with_defaults();
        let expected: &[(&str, &str, &[&str])] = &[
            ("aoe-agent", "aoe-agent", &[]),
            ("claude", "claude-agent-acp", &[]),
            ("claude-code", "claude-agent-acp", &[]),
            ("codex", "codex-acp", &[]),
            ("gemini", "gemini", &["--acp"]),
            ("kimi", "kimi", &["acp"]),
            ("omp", "omp", &["acp"]),
            ("opencode", "opencode", &["acp"]),
            ("pi", "pi-acp", &[]),
            ("prime-agent", "prime-agent", &["--mode", "acp"]),
            ("vibe", "vibe-acp", &[]),
        ];
        let listed: Vec<(&str, &str, Vec<String>)> = reg
            .list()
            .into_iter()
            .map(|(name, spec)| (name.as_str(), spec.command.as_str(), spec.args.clone()))
            .collect();
        let expected: Vec<(&str, &str, Vec<String>)> = expected
            .iter()
            .map(|(name, command, args)| (*name, *command, strings(args)))
            .collect();
        assert_eq!(listed, expected, "sorted by name, commands unchanged");
    }

    #[test]
    fn default_env_allowlists_come_from_the_binarys_catalog_entry() {
        let reg = AgentRegistry::with_defaults();
        for (name, spec) in reg.list() {
            let catalog = env_allowlist_for(&spec.command);
            let want = (!catalog.is_empty()).then(|| strings(catalog));
            assert_eq!(spec.env_allowlist, want, "{name}");
        }
        // The two Claude names share one binary, so they share one allowlist.
        assert_eq!(
            reg.get("claude").unwrap().env_allowlist,
            reg.get("claude-code").unwrap().env_allowlist
        );
        // gemini reads GEMINI_API_KEY, never the AI-Studio-only name.
        let gemini = reg.get("gemini").unwrap().env_allowlist.clone().unwrap();
        assert!(gemini.iter().any(|k| k == "GEMINI_API_KEY"));
        assert!(!gemini.iter().any(|k| k == "GOOGLE_GENERATIVE_AI_API_KEY"));

        let with_allowlist: Vec<&str> = reg
            .list()
            .into_iter()
            .filter(|(_, spec)| spec.env_allowlist.is_some())
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            with_allowlist,
            [
                "aoe-agent",
                "claude",
                "claude-code",
                "codex",
                "gemini",
                "opencode",
                "prime-agent"
            ],
            "unverified adapters (pi, omp, kimi, vibe) must stay without an env_allowlist"
        );
    }
}

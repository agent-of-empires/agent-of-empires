//! Named agent registry: maps an agent name (e.g. `claude-code`,
//! `aoe-agent`, `gemini`) to a spawn command + args. Users add agents via
//! the settings TUI; this module is the in-memory model.

use super::install_hints::{env_allowlist_for, install_hint_for, AOE_AGENT_BINARY};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Convert the static per-binary slice from `install_hints::env_allowlist_for`
/// into the owned `Option<Vec<String>>` field on `AgentSpec`. Empty slice maps
/// to `None`, the shape a deferred or custom adapter carries, so a registry
/// entry with no verified provider keys serializes as it always has.
fn default_env_allowlist(binary: &str) -> Option<Vec<String>> {
    let keys = env_allowlist_for(binary);
    (!keys.is_empty()).then(|| keys.iter().map(|s| s.to_string()).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Executable to run, e.g. `npx` or `/usr/local/bin/aoe-agent`.
    pub command: String,
    pub args: Vec<String>,
    /// Human-readable description shown in the settings TUI and
    /// `aoe acp agents`.
    pub description: String,
    /// Provider env vars forwarded to this agent on top of the infrastructure
    /// inheritance set defined by `ALWAYS_FORWARD_ENV` in `acp_client.rs`.
    /// `None` means no ambient provider credentials. Populated by
    /// `default_env_allowlist` for built-in adapters via
    /// `install_hints::env_allowlist_for`; a custom `from_acp_cmd` spec leaves
    /// it `None` and can be overridden by
    /// `session.inherit_host_environment = true` if the user needs more keys
    /// through.
    pub env_allowlist: Option<Vec<String>>,
}

impl AgentSpec {
    /// Build an ACP `AgentSpec` from a custom agent's `agent_acp_cmd`
    /// string. The string is split with shell-word rules into argv and run
    /// directly (no shell). Returns a user-facing error message when the
    /// command is empty or has malformed quoting.
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a registry seeded with one entry per aoe tool that has
    /// a published ACP server, plus our own `aoe-agent` as a generic
    /// multi-provider fallback. Each entry is keyed on the same name
    /// the tmux view uses (claude / opencode / gemini / codex /
    /// vibe / pi / omp) so the spawn path can map `instance.tool`
    /// directly to a registry key.
    ///
    /// Sources verified against
    /// <https://agentclientprotocol.com/get-started/agents.md>
    /// (Jan 2026):
    ///
    ///   claude   → claude-agent-acp     (Zed adapter for Claude SDK)
    ///   opencode → `opencode acp`       (native, SST)
    ///   gemini   → `gemini --acp`       (native, Google)
    ///   codex    → codex-acp            (ACP adapter, OpenAI Codex CLI)
    ///   vibe     → vibe-acp             (native, Mistral)
    ///   pi       → pi-acp               (adapter, Pi coding agent)
    ///   omp      → `omp acp`            (native, Oh My Pi)
    ///
    /// We deliberately don't use `npx -y` for these. First-run
    /// downloads can hang for tens of seconds with no output, which
    /// used to leave the structured view worker silently wedged before the
    /// handshake. `aoe acp doctor --fix` can install missing
    /// binaries on demand.
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();

        let claude_install = install_hint_for("claude-agent-acp").unwrap_or("(see project docs)");
        reg.agents.insert(
            "claude".into(),
            AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec![],
                description: format!(
                    "Anthropic Claude via the official ACP adapter ({claude_install})"
                ),
                env_allowlist: default_env_allowlist("claude-agent-acp"),
            },
        );
        // Legacy alias used by older session records before the
        // tool-keyed naming. Kept so persisted sessions with
        // agent_name="claude-code" still resolve.
        reg.agents.insert(
            "claude-code".into(),
            AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec![],
                description: "Alias for `claude` (legacy name)".into(),
                env_allowlist: default_env_allowlist("claude-agent-acp"),
            },
        );
        reg.agents.insert(
            "opencode".into(),
            AgentSpec {
                command: "opencode".into(),
                args: vec!["acp".into()],
                description: "OpenCode (SST), native ACP via `opencode acp`".into(),
                env_allowlist: default_env_allowlist("opencode"),
            },
        );
        reg.agents.insert(
            "gemini".into(),
            AgentSpec {
                command: "gemini".into(),
                args: vec!["--acp".into()],
                description: "Google Gemini CLI, native ACP via `gemini --acp`".into(),
                env_allowlist: default_env_allowlist("gemini"),
            },
        );
        reg.agents.insert(
            "codex".into(),
            AgentSpec {
                command: "codex-acp".into(),
                args: vec![],
                description:
                    "OpenAI Codex CLI via ACP adapter (npm i -g @agentclientprotocol/codex-acp@latest)".into(),
                env_allowlist: default_env_allowlist("codex-acp"),
            },
        );
        reg.agents.insert(
            "vibe".into(),
            AgentSpec {
                command: "vibe-acp".into(),
                args: vec![],
                description: "Mistral Vibe, native ACP via the bundled `vibe-acp` binary".into(),
                env_allowlist: default_env_allowlist("vibe-acp"),
            },
        );
        reg.agents.insert(
            "pi".into(),
            AgentSpec {
                command: "pi-acp".into(),
                args: vec![],
                description: "Pi coding agent (`pi`) via the pi-acp adapter (npm i -g pi-acp)"
                    .into(),
                env_allowlist: default_env_allowlist("pi-acp"),
            },
        );
        reg.agents.insert(
            "omp".into(),
            AgentSpec {
                command: "omp".into(),
                args: vec!["acp".into()],
                description: "Oh My Pi coding agent, native ACP via `omp acp`".into(),
                env_allowlist: default_env_allowlist("omp"),
            },
        );
        reg.agents.insert(
            "kimi".into(),
            AgentSpec {
                command: "kimi".into(),
                args: vec!["acp".into()],
                description: "Kimi Code (Moonshot AI), native ACP via `kimi acp`".into(),
                env_allowlist: default_env_allowlist("kimi"),
            },
        );
        reg.agents.insert(
            "aoe-agent".into(),
            AgentSpec {
                command: "${aoe_data_dir}/acp-worker/dist/aoe-agent".into(),
                args: vec![],
                description: "aoe's bundled multi-provider agent (Vercel AI SDK)".into(),
                // Shared binary token, NOT `command` (which carries an
                // unresolved `${aoe_data_dir}` placeholder here).
                env_allowlist: default_env_allowlist(AOE_AGENT_BINARY),
            },
        );
        reg
    }

    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.agents.get(name)
    }

    pub fn upsert(&mut self, name: String, spec: AgentSpec) {
        self.agents.insert(name, spec);
    }

    pub fn remove(&mut self, name: &str) -> Option<AgentSpec> {
        self.agents.remove(name)
    }

    pub fn list(&self) -> Vec<(&String, &AgentSpec)> {
        let mut entries: Vec<_> = self.agents.iter().collect();
        entries.sort_by_key(|(n, _)| n.as_str());
        entries
    }
}

/// If `tool` is a custom agent that inherits a built-in agent through
/// `[session.agent_detect_as]` (e.g. `lenovo-claude = claude`), and that base
/// agent has a built-in ACP adapter in the default registry, return the base
/// registry key.
///
/// This is what lets a wrapper that "inherits Claude Code" (a distinct tool
/// that only overrides profile/oauth locations) run in the structured view
/// through the base agent's adapter without the operator hand-writing an
/// `[session.agent_acp_cmd]` argv. Resolving to the *base* key (rather than the
/// wrapper name) is deliberate: the spawn then reuses the base agent's version
/// gate, env allowlist, and `AgentProfile`, so the wrapper renders in
/// structured view exactly as the base agent would. The wrapper's own identity
/// stays on `Instance.tool`, which drives status detection (via the same
/// `agent_detect_as` map) and `AOE_TOOL` host hooks.
///
/// Returns `None` when `tool` is itself a registry key (handled by the direct
/// registry lookup at every call site, which runs first), has no
/// `agent_detect_as` mapping, or maps to a base that is terminal-only
/// (e.g. `cursor`, `copilot`), which cannot back a structured session.
pub fn inherited_acp_base(tool: &str, agent_detect_as: &HashMap<String, String>) -> Option<String> {
    let base = agent_detect_as.get(tool)?;
    AgentRegistry::with_defaults()
        .get(base)
        .map(|_| base.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_claude_code_and_aoe_agent() {
        let reg = AgentRegistry::with_defaults();
        assert!(reg.get("claude-code").is_some());
        assert!(reg.get("aoe-agent").is_some());
        assert!(reg.get("omp").is_some());
    }

    #[test]
    fn from_acp_cmd_splits_argv() {
        let spec = AgentSpec::from_acp_cmd("oc-sp", "ocp run sp acp").unwrap();
        assert_eq!(spec.command, "ocp");
        assert_eq!(spec.args, vec!["run", "sp", "acp"]);
        assert_eq!(spec.description, "Custom ACP agent `oc-sp`");
        assert!(spec.env_allowlist.is_none());
    }

    #[test]
    fn from_acp_cmd_honors_quoting() {
        let spec = AgentSpec::from_acp_cmd("wrap", "sh -lc 'ocp run sp acp'").unwrap();
        assert_eq!(spec.command, "sh");
        assert_eq!(spec.args, vec!["-lc", "ocp run sp acp"]);
    }

    #[test]
    fn from_acp_cmd_rejects_empty() {
        assert!(AgentSpec::from_acp_cmd("x", "").is_err());
        assert!(AgentSpec::from_acp_cmd("x", "   ").is_err());
    }

    #[test]
    fn from_acp_cmd_rejects_unbalanced_quotes() {
        assert!(AgentSpec::from_acp_cmd("x", "ocp run \"unterminated").is_err());
    }

    /// #3238: verified adapters (`claude`, `codex`, `opencode`, `gemini`,
    /// `aoe-agent`)
    /// forward the operator's provider-auth env; adapters whose real env
    /// vars couldn't be source-verified (pi, omp, kimi, vibe) stay `None`.
    /// One row asserts a specific negative for `aoe-agent`: it must NOT
    /// receive `GEMINI_API_KEY` (that's the CLI-native name; the bundled
    /// AI-SDK agent reads `GOOGLE_GENERATIVE_AI_API_KEY` instead). The
    /// negative row catches the "keyed on `spec.command` instead of the
    /// binary token" bug, where `${aoe_data_dir}/...` would never match
    /// and every `aoe-agent` provider key would drop.
    #[test]
    fn default_env_allowlists_match_verified_providers() {
        let reg = AgentRegistry::with_defaults();
        let al = |name: &str| reg.get(name).and_then(|s| s.env_allowlist.clone());

        let claude_keys = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
        assert_eq!(al("claude").as_deref(), Some(claude_keys.as_slice()));
        assert_eq!(al("claude-code").as_deref(), Some(claude_keys.as_slice()));
        assert_eq!(
            al("codex").as_deref(),
            Some(
                &[
                    "CODEX_API_KEY",
                    "OPENAI_API_KEY",
                    "OPENAI_BASE_URL",
                    "CODEX_HOME"
                ][..]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()[..]
            )
        );
        assert_eq!(
            al("aoe-agent").as_deref(),
            Some(
                &[
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "OPENAI_BASE_URL",
                    "GOOGLE_GENERATIVE_AI_API_KEY"
                ][..]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()[..]
            )
        );
        // gemini CLI uses the CLI-native GEMINI_API_KEY; aoe-agent (AI-SDK)
        // does not. Verify the two do not share the wrong Google key.
        let gemini = al("gemini").unwrap_or_default();
        assert!(gemini.iter().any(|k| k == "GEMINI_API_KEY"));
        assert!(!gemini.iter().any(|k| k == "GOOGLE_GENERATIVE_AI_API_KEY"));
        // Vertex is dead without the switch that selects it and the region
        // that pairs with the project, so the credential alone is not enough.
        for key in [
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
        ] {
            assert!(gemini.iter().any(|k| k == key), "gemini missing {key}");
        }
        let aoe = al("aoe-agent").unwrap_or_default();
        assert!(!aoe.iter().any(|k| k == "GEMINI_API_KEY"));

        // opencode resolves providers through models.dev, whose `google` entry
        // declares all three Google key names, so a user with any one of them
        // exported must authenticate.
        let opencode = al("opencode").unwrap_or_default();
        for key in [
            // The `anthropic` entry declares this one; opencode against a
            // Claude model reads it from the environment like any other
            // models.dev provider key.
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GOOGLE_API_KEY",
            "GEMINI_API_KEY",
        ] {
            assert!(opencode.iter().any(|k| k == key), "opencode missing {key}");
        }

        // Deferred adapters stay None until each adapter's env reads are
        // verified from its own source.
        for name in ["pi", "omp", "kimi", "vibe"] {
            assert!(
                al(name).is_none(),
                "{name} must have None env_allowlist until source-verified"
            );
        }

        // Structural link between the registry and `env_allowlist_for`: exactly
        // these adapters carry an allowlist. Adding an arm to `env_allowlist_for`
        // (or a new default agent) without updating this set, or dropping an
        // existing arm, fails here rather than silently changing what a spawn
        // forwards.
        let with_allowlist: std::collections::BTreeSet<&str> = reg
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
            ]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the set of adapters with an env_allowlist changed; update env_allowlist_for and this assertion together"
        );
    }

    #[test]
    fn inherited_acp_base_resolves_only_registry_backed_bases() {
        let mut detect_as = HashMap::new();
        // Wrapper inheriting a base that has an ACP adapter → resolves to base.
        detect_as.insert("lenovo-claude".to_string(), "claude".to_string());
        detect_as.insert("work-codex".to_string(), "codex".to_string());
        // Wrapper inheriting a terminal-only base (no ACP adapter) → None.
        detect_as.insert("my-cursor".to_string(), "cursor".to_string());
        // Base is another custom name, not a registry key → None.
        detect_as.insert("chain".to_string(), "lenovo-claude".to_string());

        let cases = [
            ("lenovo-claude", Some("claude")),
            ("work-codex", Some("codex")),
            ("my-cursor", None),
            ("chain", None),
            // No mapping at all.
            ("unmapped", None),
        ];
        for (tool, expected) in cases {
            assert_eq!(
                inherited_acp_base(tool, &detect_as).as_deref(),
                expected,
                "{tool}"
            );
        }
    }

    #[test]
    fn list_is_sorted() {
        let mut reg = AgentRegistry::new();
        reg.upsert(
            "zeta".into(),
            AgentSpec {
                command: "z".into(),
                args: vec![],
                description: "z".into(),
                env_allowlist: None,
            },
        );
        reg.upsert(
            "alpha".into(),
            AgentSpec {
                command: "a".into(),
                args: vec![],
                description: "a".into(),
                env_allowlist: None,
            },
        );
        let names: Vec<&str> = reg.list().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }
}

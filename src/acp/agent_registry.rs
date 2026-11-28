//! Named agent registry: maps an agent name (e.g. `claude`, `aoe-agent`) to a spawn command.

use super::acp_client::ProxySpec;
use super::install_hints::{install_hint_for, AOE_AGENT_BINARY};
use super::resolved_agent::{AcpAgentSource, ResolvedAcpAgent};
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
    /// The env a plugin declared for its adapter: its `defaults` replace the built-in adapter
    /// defaults and its `host_only` keys stay out of a sandbox. `None` for a built-in or custom agent.
    #[serde(default)]
    pub plugin_env: Option<aoe_plugin_api::agent_integration::AcpEnv>,
}

impl AgentSpec {
    /// Allowlisted keys a plugin declared meaningless in a container.
    pub(crate) fn host_only_env(&self) -> &[String] {
        self.plugin_env
            .as_ref()
            .map_or(&[], |env| env.host_only.as_slice())
    }

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
            plugin_env: None,
        })
    }
}

/// True the first time a `(plugin, agent, reason)` is seen, so a registry rebuilt per
/// resolution does not repeat the warning.
fn first_warning(plugin: &str, agent: &str, reason: &str) -> bool {
    static WARNED: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashSet<(String, String, String)>>,
    > = std::sync::LazyLock::new(Default::default);
    WARNED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert((plugin.to_owned(), agent.to_owned(), reason.to_owned()))
}

/// The outcome of [`AgentRegistry::resolve_spec`].
#[derive(Debug)]
pub struct SpawnSpec {
    pub spec: AgentSpec,
    /// False for the user's own `agent_acp_cmd`.
    pub from_registry: bool,
    pub proxy: Option<ProxySpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRegistry {
    pub agents: HashMap<String, ResolvedAcpAgent>,
}

impl AgentRegistry {
    /// One entry per tool with a published ACP server, plus the built-in agent, plus the
    /// approved plugins' ACP agents from one snapshot of the plugin registry.
    pub fn with_defaults() -> Self {
        Self::with_snapshot(&crate::plugin::registry())
    }

    pub fn with_snapshot(plugins: &crate::plugin::PluginRegistry) -> Self {
        let mut registry = Self::builtin();
        registry.merge_plugin_agents(plugins);
        registry
    }

    fn builtin() -> Self {
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
                (
                    name.to_string(),
                    ResolvedAcpAgent::builtin(name, command, args, description),
                )
            })
            .collect();
        Self { agents }
    }

    /// Add each active plugin's `acp` declaration that passes the same gate as plugin launch
    /// declarations. Plugin-vs-plugin name collisions drop both sides. A takeover of a bundled
    /// tool replaces the built-in rows paired to it; any other built-in id or alias wins.
    fn merge_plugin_agents(&mut self, plugins: &crate::plugin::PluginRegistry) {
        use crate::plugin::agent_integration::acp_declaration_applies;
        let candidates: Vec<_> = plugins
            .active()
            .flat_map(|plugin| {
                plugin
                    .manifest
                    .agent_integrations
                    .iter()
                    .filter(move |entry| {
                        entry.acp.is_some() && acp_declaration_applies(plugin, entry)
                    })
                    .map(move |entry| (plugin, entry))
            })
            .filter_map(|(plugin, entry)| Some((plugin, entry.tool.as_str(), entry.acp.as_ref()?)))
            .collect();
        let claims = |name: &str| {
            candidates
                .iter()
                .filter(|(_, _, acp)| acp.id == name || acp.aliases.iter().any(|a| a == name))
                .count()
        };
        // Every name the built-in rows answer to, taken before a takeover removes any row.
        let reserved: std::collections::HashSet<String> = self
            .agents
            .values()
            .flat_map(|builtin| {
                std::iter::once(&builtin.id)
                    .chain(&builtin.aliases)
                    .chain(&builtin.paired_tool)
                    .cloned()
            })
            .collect();
        for (plugin, tool, acp) in &candidates {
            let ambiguous = std::iter::once(&acp.id)
                .chain(&acp.aliases)
                .any(|name| claims(name) > 1)
                || candidates
                    .iter()
                    .filter(|(_, other, _)| other == tool)
                    .count()
                    > 1;
            if ambiguous {
                if first_warning(plugin.id(), &acp.id, "ambiguous") {
                    tracing::warn!(target: "plugin.agents", plugin = plugin.id(), %tool, agent = %acp.id,
                        "ACP agent id, alias or tool is claimed by more than one plugin; ignoring it");
                }
                continue;
            }
            let replaced: Vec<String> = if crate::plugin::bundled_agents::contains(tool) {
                self.agents
                    .values()
                    .filter(|builtin| builtin.paired_tool.as_deref() == Some(*tool))
                    .map(|builtin| builtin.id.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let bundled = replaced
                .iter()
                .filter_map(|id| self.agents.get(id))
                .min_by_key(|builtin| (builtin.id != *tool, builtin.id.as_str()))
                .map(|builtin| builtin.spec.clone());
            let agent = match ResolvedAcpAgent::from_plugin(
                plugin.id(),
                plugin.dir.as_deref(),
                tool,
                acp,
                bundled.as_ref(),
            ) {
                Ok(agent) => agent,
                Err(error) => {
                    if first_warning(plugin.id(), &acp.id, &error) {
                        tracing::warn!(target: "plugin.agents", plugin = plugin.id(), %tool, agent = %acp.id, %error,
                            "plugin ACP agent cannot be resolved; ignoring it");
                    }
                    continue;
                }
            };
            let taken = std::iter::once(&agent.id)
                .chain(&agent.aliases)
                .find(|name| reserved.contains(*name) && !replaced.contains(name));
            if let Some(name) = taken {
                if first_warning(plugin.id(), &agent.id, &format!("collides:{name}")) {
                    tracing::warn!(target: "plugin.agents", plugin = plugin.id(), agent = %agent.id, existing = %name,
                        "plugin ACP agent collides with a built-in name; preserving it");
                }
                continue;
            }
            for id in replaced {
                self.agents.remove(&id);
            }
            self.agents.insert(agent.id.clone(), agent);
        }
    }

    /// The agent `name` addresses: by id, by alias, else the agent paired to the tool `name`
    /// (`claude-code` is the legacy name of tool `claude`), so a takeover that removed the
    /// built-in rows still answers to their persisted names.
    pub fn resolved(&self, name: &str) -> Option<&ResolvedAcpAgent> {
        self.agents
            .get(name)
            .or_else(|| self.agents.values().find(|agent| agent.answers_to(name)))
            .or_else(|| {
                self.paired_agent(if name == "claude-code" {
                    "claude"
                } else {
                    name
                })
            })
    }

    /// A built-in row named `name`, which no plugin or user config can shadow.
    fn builtin_row(&self, name: &str) -> Option<&ResolvedAcpAgent> {
        self.agents
            .get(name)
            .filter(|agent| agent.source == AcpAgentSource::Builtin)
    }

    /// The built-in or plugin base `tool` inherits via `agent_detect_as`.
    pub fn inherited_base(
        &self,
        tool: &str,
        agent_detect_as: &HashMap<String, String>,
    ) -> Option<String> {
        let base = agent_detect_as.get(tool)?;
        self.get(base).map(|_| base.clone())
    }

    /// The registry row whose adapter a spawn of `name` runs, by the precedence of
    /// [`Self::resolve_spec`]; `None` when the user's own command or no row serves it.
    pub(crate) fn launched_row(
        &self,
        name: &str,
        session: &crate::session::config::SessionConfig,
    ) -> Option<&ResolvedAcpAgent> {
        if let Some(agent) = self.builtin_row(name) {
            return Some(agent);
        }
        if session.agent_acp_cmd.contains_key(name) {
            return None;
        }
        if let Some(base) = self.inherited_base(name, &session.agent_detect_as) {
            return self.resolved(&base);
        }
        self.resolved(name)
    }

    /// What a spawn of `name` runs. A built-in row wins, then the user's `agent_acp_cmd` and
    /// `agent_detect_as` base, then a plugin row, so a plugin never shadows a name the user
    /// configured. Only a row addressed by its own name brings its proxy.
    pub fn resolve_spec(
        &self,
        name: &str,
        session: &crate::session::config::SessionConfig,
    ) -> Result<Option<SpawnSpec>, String> {
        let from_registry = |spec: &AgentSpec, proxy: Option<&ProxySpec>| SpawnSpec {
            spec: spec.clone(),
            from_registry: true,
            proxy: proxy.cloned(),
        };
        if let Some(agent) = self.builtin_row(name) {
            return Ok(Some(from_registry(&agent.spec, None)));
        }
        if let Some(cmd) = session.agent_acp_cmd.get(name) {
            return AgentSpec::from_acp_cmd(name, cmd).map(|spec| {
                Some(SpawnSpec {
                    spec,
                    from_registry: false,
                    proxy: None,
                })
            });
        }
        if let Some(spec) = self
            .inherited_base(name, &session.agent_detect_as)
            .and_then(|base| self.get(&base))
        {
            return Ok(Some(from_registry(spec, None)));
        }
        Ok(self
            .resolved(name)
            .map(|agent| from_registry(&agent.spec, agent.proxy.as_ref())))
    }

    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.resolved(name).map(|agent| &agent.spec)
    }

    /// The agent paired to `tool`: the one whose id is the tool's own name when several are.
    pub fn paired_agent(&self, tool: &str) -> Option<&ResolvedAcpAgent> {
        self.agents
            .values()
            .filter(|agent| agent.paired_tool.as_deref() == Some(tool))
            .min_by_key(|agent| (agent.id != tool, agent.id.as_str()))
    }

    /// The agent a structured session of `tool` runs: its paired agent, else one named `tool`.
    pub fn for_tool(&self, tool: &str) -> Option<&ResolvedAcpAgent> {
        self.paired_agent(tool).or_else(|| self.resolved(tool))
    }

    /// Entries sorted by name.
    pub fn list(&self) -> Vec<(&String, &AgentSpec)> {
        let mut entries: Vec<_> = self.agents.iter().map(|(n, a)| (n, &a.spec)).collect();
        entries.sort_by_key(|(n, _)| n.as_str());
        entries
    }
}

/// The built-in base `tool` inherits via `agent_detect_as`, when that base has an ACP adapter.
pub fn inherited_acp_base(tool: &str, agent_detect_as: &HashMap<String, String>) -> Option<String> {
    AgentRegistry::with_defaults().inherited_base(tool, agent_detect_as)
}

/// Whether `agent` always runs in Yolo mode in `view`: a built-in that does, or a custom agent
/// whose base does. A terminal session runs the `agent_detect_as` base CLI; a structured one
/// runs the agent's own valid adapter when it has one, else the base's.
pub fn runs_always_yolo(
    agent: &str,
    view: crate::session::View,
    agent_acp_cmd: &HashMap<String, String>,
    agent_detect_as: &HashMap<String, String>,
) -> bool {
    if crate::agents::get_agent(agent).is_some() {
        return crate::agents::is_always_yolo(agent);
    }
    match view {
        crate::session::View::Terminal => agent_detect_as
            .get(agent)
            .is_some_and(|base| crate::agents::is_always_yolo(base)),
        crate::session::View::Structured => {
            let own_adapter = agent_acp_cmd
                .get(agent)
                .is_some_and(|cmd| AgentSpec::from_acp_cmd(agent, cmd).is_ok());
            !own_adapter
                && inherited_acp_base(agent, agent_detect_as)
                    .is_some_and(|base| crate::agents::is_always_yolo(&base))
        }
    }
}

/// The name users know a registry agent by, for headlines such as
/// "Codex couldn't start".
pub fn display_name_for(agent: &str) -> Option<&'static str> {
    match agent {
        "claude-code" => crate::agents::native_definition("claude"),
        "aoe-agent" => return Some("aoe-agent"),
        _ => crate::agents::native_definition(agent),
    }
    .map(|def| def.display_name)
}

/// The agent a structured-view session of `tool` spawns as: an explicit override, the tool's
/// built-in row, the user's custom command or inherited base, the plugin agent for the tool,
/// then the configured default agent.
pub fn pick_acp_agent_name(
    registry: &AgentRegistry,
    session: &crate::session::config::SessionConfig,
    acp: &crate::session::config::AcpConfig,
    tool: &str,
    explicit_override: Option<&str>,
) -> String {
    pick_acp_agent_name_with_default(
        registry,
        session,
        acp.resolved_default_agent(),
        tool,
        explicit_override,
    )
}

pub(crate) fn pick_acp_agent_name_with_default(
    registry: &AgentRegistry,
    session: &crate::session::config::SessionConfig,
    default_agent: &str,
    tool: &str,
    explicit_override: Option<&str>,
) -> String {
    if let Some(name) = explicit_override.filter(|name| !name.is_empty()) {
        return name.to_string();
    }
    if let Some(agent) = registry.builtin_row(tool) {
        return agent.id.clone();
    }
    let custom_cmd = session
        .agent_acp_cmd
        .get(tool)
        .is_some_and(|cmd| AgentSpec::from_acp_cmd(tool, cmd).is_ok());
    if custom_cmd {
        return tool.to_string();
    }
    if let Some(base) = registry.inherited_base(tool, &session.agent_detect_as) {
        return base;
    }
    if let Some(agent) = registry.for_tool(tool) {
        return agent.id.clone();
    }
    default_agent.to_string()
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
    use crate::acp::install_hints::env_allowlist_for;

    fn strings(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    fn acp_plugin(
        plugin_id: &str,
        tool: &str,
        acp_id: &str,
        aliases: &[&str],
        dir: &std::path::Path,
    ) -> crate::plugin::LoadedPlugin {
        let aliases = aliases
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>();
        let manifest = format!(
            "id = \"{plugin_id}\"\nname = \"Acp\"\nversion = \"1.0.0\"\napi_version = 30\n\
             capabilities = [\"acp.agent.provide\"]\n\
             [[agent_integrations]]\ntool = \"{tool}\"\n\
             [agent_integrations.acp]\nid = \"{acp_id}\"\naliases = [{}]\n\
             display_name = \"Acp\"\nfailure_policy = \"refuse\"\n\
             [agent_integrations.acp.proxy]\ncommand = \"bin/proxy\"\n\
             protocol_version = 1\nprimitive_schema_version = 1\n\
             [agent_integrations.acp.env]\nallowlist = [\"ZZ_KEY\", \"LD_PRELOAD\"]\n",
            aliases.join(", ")
        );
        crate::plugin::LoadedPlugin {
            manifest: toml::from_str(&manifest).unwrap(),
            enabled: true,
            trust: aoe_plugin_api::TrustLevel::Community,
            validation: crate::plugin::registry::ValidationState::Local,
            source: None,
            dir: Some(dir.to_path_buf()),
            manifest_hash: String::new(),
            granted: true,
        }
    }

    fn plugin_dir(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/proxy"), "").unwrap();
        dir
    }

    fn registry_of(plugins: Vec<crate::plugin::LoadedPlugin>) -> AgentRegistry {
        AgentRegistry::with_snapshot(&crate::plugin::PluginRegistry::from_test_plugins(plugins))
    }

    #[test]
    fn builtin_rows_pair_to_their_tool_and_claude_code_to_claude() {
        let registry = registry_of(Vec::new());
        let paired = |id: &str| registry.resolved(id).unwrap().paired_tool.clone();
        assert_eq!(paired("claude").as_deref(), Some("claude"));
        assert_eq!(paired("claude-code").as_deref(), Some("claude"));
        assert_eq!(paired("codex").as_deref(), Some("codex"));
        assert_eq!(paired("aoe-agent"), None);
        assert_eq!(registry.paired_agent("claude").unwrap().id, "claude");
        assert!(registry.paired_agent("aoe-agent").is_none());
        assert!(registry
            .agents
            .values()
            .all(|a| a.source == AcpAgentSource::Builtin));
    }

    #[test]
    fn plugin_acp_agent_resolves_by_id_and_alias_and_pairs_to_its_tool() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let registry = registry_of(vec![acp_plugin(
            "acme.zeta",
            "zzzeta",
            "zeta-acp",
            &["zeta-cli"],
            &dir,
        )]);

        let agent = registry.paired_agent("zzzeta").unwrap();
        assert_eq!(agent.id, "zeta-acp");
        assert_eq!(
            agent.source,
            crate::acp::resolved_agent::AcpAgentSource::Plugin {
                plugin_id: "acme.zeta".into()
            }
        );
        assert_eq!(registry.resolved("zeta-cli").unwrap().id, "zeta-acp");
        assert_eq!(agent.env.allowlist, ["ZZ_KEY"]);
        // The tool name addresses its paired agent too.
        assert_eq!(registry.get("zzzeta").unwrap().command, "zzzeta");
    }

    #[test]
    fn plugin_agent_launches_its_adapter_behind_the_declared_proxy() {
        use aoe_plugin_api::agent_integration::{AcpAdapter, AcpCompat, AcpFailurePolicy};
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let declared = |tool: &str, id: &str, plugin: &str| {
            let mut loaded = acp_plugin(plugin, tool, id, &[], &dir);
            let acp = loaded.manifest.agent_integrations[0].acp.as_mut().unwrap();
            acp.failure_policy = AcpFailurePolicy::NeutralBoot;
            acp.compat = Some(AcpCompat {
                expected_agent_name: Some("@acme/zeta-acp".into()),
                min_version: Some("2.0.0".into()),
                capability_min_versions: Default::default(),
            });
            acp.adapter = Some(AcpAdapter {
                upstream_package: "@acme/zeta-acp".into(),
                upstream_pin: "2.1.0".into(),
                package_dir: "adapter".into(),
                binary: None,
            });
            loaded
        };
        let registry = registry_of(vec![declared("zzzeta", "zeta-acp", "acme.zeta")]);
        let agent = registry.resolved("zeta-acp").unwrap();
        // The registry and launch command is the adapter; the proxy wraps it.
        assert_eq!(agent.spec.command, "zeta-acp");
        assert!(agent.spec.args.is_empty());
        let proxy = agent.proxy.as_ref().unwrap();
        assert!(proxy.command.ends_with("acme/bin/proxy"));
        assert_eq!(proxy.plugin_id, "acme.zeta");
        assert_eq!(proxy.display_name, "Acp");
        assert!(proxy.args.is_empty());
        assert_eq!(
            proxy.failure_policy,
            crate::acp::acp_client::ProxyFailurePolicy::NeutralBoot
        );
        assert_eq!(proxy.compat.agent_names, ["@acme/zeta-acp"]);
        assert_eq!(
            proxy.compat.version_req.as_ref().unwrap().to_string(),
            ">=2.0.0"
        );
        assert_eq!(
            proxy.adapter.min_version.as_ref().unwrap().to_string(),
            "2.0.0"
        );
        assert!(registry.resolved("claude").unwrap().proxy.is_none());

        // Without a declared adapter: the built-in adapter being taken over, else the tool name.
        let mut takeover = acp_plugin("aoe.takeover", "claude", "claude-next", &[], &dir);
        takeover.manifest.agent_integrations[0]
            .acp
            .as_mut()
            .unwrap()
            .adapter = None;
        let plain = acp_plugin("acme.plain", "zzplain", "plain", &[], &dir);
        let registry = registry_of(vec![takeover, plain]);
        assert_eq!(
            registry.resolved("claude-next").unwrap().spec.command,
            "claude-agent-acp"
        );
        assert!(registry.resolved("claude-next").unwrap().proxy.is_some());
        assert_eq!(registry.resolved("plain").unwrap().spec.command, "zzplain");
    }

    #[test]
    fn plugin_agents_cannot_displace_builtins_without_the_reserved_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        // A plugin tool that is a built-in name, and an id that is a built-in id.
        let registry = registry_of(vec![
            acp_plugin("acme.one", "claude", "claude-next", &[], &dir),
            acp_plugin("acme.two", "opencode", "oc-next", &[], &dir),
            acp_plugin("aoe.three", "opencode", "oc-three", &[], &dir),
            acp_plugin("acme.four", "zzfour", "codex", &[], &dir),
            acp_plugin("acme.five", "zzfive", "five", &["claude-code"], &dir),
        ]);
        assert!(registry
            .agents
            .values()
            .all(|a| a.source == AcpAgentSource::Builtin));
        assert_eq!(registry.paired_agent("claude").unwrap().id, "claude");
        assert_eq!(registry.paired_agent("opencode").unwrap().id, "opencode");
        assert!(registry.paired_agent("zzfive").is_none());
        assert_eq!(registry.get("codex").unwrap().command, "codex-acp");

        let registry = registry_of(vec![acp_plugin(
            "aoe.claude",
            "claude",
            "claude-next",
            &["claude-code"],
            &dir,
        )]);
        assert_eq!(registry.paired_agent("claude").unwrap().id, "claude-next");
        assert_eq!(registry.resolved("claude-code").unwrap().id, "claude-next");
        assert!(!registry.agents.contains_key("claude"));
        assert!(!registry.agents.contains_key("claude-code"));
    }

    #[test]
    fn a_community_plugin_cannot_claim_a_name_a_takeover_freed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let takeover = || acp_plugin("aoe.claude", "claude", "claude-next", &[], &dir);
        let community = || acp_plugin("acme.helper", "zzhelper", "claude", &[], &dir);
        for plugins in [vec![takeover(), community()], vec![community(), takeover()]] {
            let registry = registry_of(plugins);
            assert_eq!(registry.resolved("claude").unwrap().id, "claude-next");
            assert!(registry.resolved("zzhelper").is_none());
            assert!(!registry.agents.contains_key("claude"));
        }
    }

    #[test]
    fn plugin_vs_plugin_collisions_drop_both_sides() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        for (a, b) in [
            (("zza", "same", &[][..]), ("zzb", "same", &[][..])),
            (
                ("zza", "one", &["shared"][..]),
                ("zzb", "two", &["shared"][..]),
            ),
            (("zza", "one", &[][..]), ("zza", "two", &[][..])),
        ] {
            let registry = registry_of(vec![
                acp_plugin("acme.a", a.0, a.1, a.2, &dir),
                acp_plugin("acme.b", b.0, b.1, b.2, &dir),
                acp_plugin("acme.c", "zzc", "ok", &[], &dir),
            ]);
            let plugin_ids: Vec<_> = registry
                .agents
                .values()
                .filter(|agent| agent.source != AcpAgentSource::Builtin)
                .map(|agent| agent.id.as_str())
                .collect();
            assert_eq!(plugin_ids, ["ok"], "{a:?} {b:?}");
        }
    }

    #[test]
    fn unapproved_or_unresolvable_plugin_agents_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let mut disabled = acp_plugin("acme.off", "zzoff", "off", &[], &dir);
        disabled.enabled = false;
        let mut ungranted = acp_plugin("acme.ungranted", "zzun", "ungranted", &[], &dir);
        ungranted.granted = false;
        let mut no_dir = acp_plugin("acme.nodir", "zznd", "nodir", &[], &dir);
        no_dir.dir = None;
        let mut missing = acp_plugin("acme.missing", "zzms", "missing", &[], &dir);
        missing.dir = Some(temp.path().join("nowhere"));
        let registry = registry_of(vec![disabled, ungranted, no_dir, missing]);
        assert!(registry
            .agents
            .values()
            .all(|a| a.source == AcpAgentSource::Builtin));

        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            std::fs::write(&outside, "").unwrap();
            let escape = plugin_dir(temp.path(), "escape");
            std::fs::remove_file(escape.join("bin/proxy")).unwrap();
            std::os::unix::fs::symlink(&outside, escape.join("bin/proxy")).unwrap();
            let registry = registry_of(vec![acp_plugin("acme.esc", "zzesc", "esc", &[], &escape)]);
            assert!(registry.get("esc").is_none());
        }
    }

    #[test]
    fn pick_follows_the_paired_agent_of_a_plugin_tool() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let registry = registry_of(vec![acp_plugin(
            "acme.zeta",
            "zzzeta",
            "zeta-acp",
            &[],
            &dir,
        )]);
        let session = crate::session::config::SessionConfig::default();
        let acp = crate::session::config::AcpConfig::default();
        let default = acp.resolved_default_agent().to_string();
        for (tool, explicit, want) in [
            ("zzzeta", None, "zeta-acp"),
            ("zzzeta", Some("gemini"), "gemini"),
            ("claude", None, "claude"),
            ("unknown", None, default.as_str()),
        ] {
            assert_eq!(
                pick_acp_agent_name(&registry, &session, &acp, tool, explicit),
                want,
                "{tool} / {explicit:?}"
            );
        }
    }

    #[test]
    fn a_plugin_never_shadows_a_name_the_user_configured() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let registry = registry_of(vec![
            acp_plugin("acme.zeta", "zzzeta", "zeta-acp", &["zeta-cli"], &dir),
            acp_plugin("acme.mine", "zzmine", "mine", &[], &dir),
        ]);
        let session = crate::session::config::SessionConfig {
            agent_acp_cmd: [
                ("zeta-cli".to_string(), "my-zeta acp".to_string()),
                ("zzzeta".to_string(), "my-zzzeta".to_string()),
            ]
            .into(),
            agent_detect_as: [("mine".to_string(), "claude".to_string())].into(),
            ..Default::default()
        };
        let acp = crate::session::config::AcpConfig::default();

        for (tool, want) in [
            // The user's command for the tool outranks the plugin paired to it.
            ("zzzeta", "zzzeta"),
            ("mine", "claude"),
            ("zzmine", "mine"),
            ("claude", "claude"),
        ] {
            assert_eq!(
                pick_acp_agent_name(&registry, &session, &acp, tool, None),
                want,
                "{tool}"
            );
        }
        // Without the user's config the plugin answers.
        let bare = crate::session::config::SessionConfig::default();
        assert_eq!(
            pick_acp_agent_name(&registry, &bare, &acp, "zzzeta", None),
            "zeta-acp"
        );

        // (name, command, from the registry, carries the plugin proxy)
        for (name, command, from_registry, proxied) in [
            ("zeta-cli", "my-zeta", false, false),
            ("mine", "claude-agent-acp", true, false),
            ("zeta-acp", "zzzeta", true, true),
            ("zzmine", "zzmine", true, true),
            ("claude", "claude-agent-acp", true, false),
        ] {
            let got = registry.resolve_spec(name, &session).unwrap().unwrap();
            assert_eq!(got.spec.command, command, "{name}");
            assert_eq!(got.from_registry, from_registry, "{name}");
            assert_eq!(got.proxy.is_some(), proxied, "{name}");
        }
        assert!(registry.resolve_spec("nope", &session).unwrap().is_none());
    }

    #[test]
    fn a_takeover_keeps_answering_to_the_persisted_builtin_names() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(temp.path(), "acme");
        let session = crate::session::config::SessionConfig::default();
        for (aliases, expected_alias) in [(&[][..], false), (&["claude-code"][..], true)] {
            let registry = registry_of(vec![acp_plugin(
                "aoe.claude",
                "claude",
                "claude-next",
                aliases,
                &dir,
            )]);
            // Only a declared alias is the plugin's; otherwise the legacy name follows the tool.
            let declared = registry
                .agents
                .values()
                .any(|agent| agent.aliases.iter().any(|a| a == "claude-code"));
            assert_eq!(declared, expected_alias);
            for name in ["claude", "claude-code"] {
                assert_eq!(
                    registry.get(name).unwrap().command,
                    "claude-agent-acp",
                    "{name}"
                );
                assert_eq!(registry.for_tool(name).unwrap().id, "claude-next", "{name}");
                let got = registry.resolve_spec(name, &session).unwrap().unwrap();
                assert!(got.from_registry && got.proxy.is_some(), "{name}");
            }
        }
    }

    #[test]
    fn plugin_warnings_are_reported_once_per_reason() {
        assert!(first_warning("acme.once", "agent", "reason"));
        assert!(!first_warning("acme.once", "agent", "reason"));
        assert!(first_warning("acme.once", "agent", "another reason"));
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
            let with_config = pick_acp_agent_name(&registry, &session, &acp, tool, explicit);
            assert_eq!(with_config, want, "{tool} / {explicit:?}");
            assert_eq!(
                pick_acp_agent_name_with_default(
                    &registry,
                    &session,
                    acp.resolved_default_agent(),
                    tool,
                    explicit,
                ),
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
    fn every_agent_has_a_display_name() {
        for (name, _) in AgentRegistry::with_defaults().list() {
            assert!(display_name_for(name).is_some(), "{name}");
        }
        for agent in crate::agents::native_agents() {
            assert!(display_name_for(agent.name).is_some(), "{}", agent.name);
        }
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

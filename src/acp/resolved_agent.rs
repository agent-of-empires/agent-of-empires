//! Host-normalized ACP agent: one shape for built-in rows and plugin declarations.

use super::acp_client::{provider_env_denyreason, ProxyFailurePolicy};
use super::agent_registry::AgentSpec;
use super::install_hints::env_allowlist_for;
use aoe_plugin_api::agent_integration::{
    AcpAdapter, AcpCompat, AcpDeclaration, AcpEnv, AcpFailurePolicy, AcpMode,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpAgentSource {
    Builtin,
    Plugin { plugin_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAcpAgent {
    pub id: String,
    pub aliases: Vec<String>,
    pub display_name: String,
    /// The tool whose structured sessions spawn this agent; `None` for an agent no tool names.
    pub paired_tool: Option<String>,
    pub source: AcpAgentSource,
    /// Launch argv (`command` plus `args`, never run through a shell), description and the
    /// env allowlist the spawn path reads.
    pub spec: AgentSpec,
    /// Sanitized: no key the host denies.
    pub env: AcpEnv,
    pub compat: Option<AcpCompat>,
    pub modes: Vec<AcpMode>,
    pub failure_policy: ProxyFailurePolicy,
    pub adapter: Option<AcpAdapter>,
}

impl ResolvedAcpAgent {
    pub(super) fn builtin(
        id: &str,
        command: &str,
        args: &[&str],
        description: &str,
    ) -> ResolvedAcpAgent {
        let allowlist: Vec<String> = env_allowlist_for(command)
            .iter()
            .map(|key| key.to_string())
            .collect();
        let (display_name, paired_tool) = match id {
            "claude-code" => (crate::acp::display_name_for(id), Some("claude")),
            "aoe-agent" => (crate::acp::display_name_for(id), None),
            _ => (crate::acp::display_name_for(id), Some(id)),
        };
        ResolvedAcpAgent {
            id: id.into(),
            aliases: Vec::new(),
            display_name: display_name.unwrap_or(id).into(),
            paired_tool: paired_tool.map(str::to_string),
            source: AcpAgentSource::Builtin,
            spec: AgentSpec {
                command: command.into(),
                args: args.iter().map(|a| a.to_string()).collect(),
                description: description.into(),
                env_allowlist: (!allowlist.is_empty()).then(|| allowlist.clone()),
            },
            env: AcpEnv {
                allowlist,
                ..AcpEnv::default()
            },
            compat: None,
            modes: Vec::new(),
            failure_policy: ProxyFailurePolicy::Refuse,
            adapter: None,
        }
    }

    /// Resolve an approved plugin's `acp` part. `plugin_dir` is the install root the proxy
    /// must stay beneath.
    pub(crate) fn from_plugin(
        plugin_id: &str,
        plugin_dir: Option<&Path>,
        tool: &str,
        acp: &AcpDeclaration,
    ) -> Result<ResolvedAcpAgent, String> {
        let dir = plugin_dir.ok_or("plugin has no install directory")?;
        let command = resolve_beneath(dir, &acp.proxy.command)?;
        let env = sanitize_env(&acp.env);
        Ok(ResolvedAcpAgent {
            id: acp.id.clone(),
            aliases: acp.aliases.clone(),
            display_name: acp.display_name.clone(),
            paired_tool: Some(tool.to_string()),
            source: AcpAgentSource::Plugin {
                plugin_id: plugin_id.to_string(),
            },
            spec: AgentSpec {
                command: command.to_string_lossy().into_owned(),
                args: Vec::new(),
                description: format!("{} (plugin {plugin_id})", acp.display_name),
                env_allowlist: (!env.allowlist.is_empty()).then(|| env.allowlist.clone()),
            },
            env,
            compat: acp.compat.clone(),
            modes: acp.profile.modes.clone(),
            failure_policy: match acp.failure_policy {
                AcpFailurePolicy::NeutralBoot => ProxyFailurePolicy::NeutralBoot,
                AcpFailurePolicy::Refuse => ProxyFailurePolicy::Refuse,
            },
            adapter: acp.adapter.clone(),
        })
    }

    pub(crate) fn answers_to(&self, name: &str) -> bool {
        self.id == name || self.aliases.iter().any(|alias| alias == name)
    }
}

/// `rel` under `root`, with symlinks resolved: a link or `..` leaving the root is refused.
fn resolve_beneath(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("install directory is unreadable ({e})"))?;
    let path = root
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("proxy command {rel:?} is unreadable ({e})"))?;
    if path.starts_with(&root) {
        Ok(path)
    } else {
        Err(format!(
            "proxy command {rel:?} escapes the install directory"
        ))
    }
}

fn env_key_allowed(key: &str) -> bool {
    crate::session::environment::is_valid_env_key(key)
        && !key.starts_with("AOE_")
        && provider_env_denyreason(key).is_none()
}

/// Drop every key the host denies; a plugin cannot widen that set.
fn sanitize_env(env: &AcpEnv) -> AcpEnv {
    let allowlist: Vec<String> = env
        .allowlist
        .iter()
        .filter(|key| env_key_allowed(key))
        .cloned()
        .collect();
    AcpEnv {
        host_only: env
            .host_only
            .iter()
            .filter(|key| allowlist.contains(key))
            .cloned()
            .collect(),
        defaults: env
            .defaults
            .iter()
            .filter(|(key, _)| env_key_allowed(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        allowlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_policy_drops_host_denied_keys() {
        let env = AcpEnv {
            allowlist: [
                "EXAMPLE_KEY",
                "AOE_TOKEN",
                "AOE_ANYTHING",
                "LD_PRELOAD",
                "DYLD_X",
                "PATH",
                "bad-name",
            ]
            .map(String::from)
            .to_vec(),
            host_only: ["EXAMPLE_KEY", "LD_PRELOAD"].map(String::from).to_vec(),
            defaults: [
                ("EXAMPLE_REGION", "eu"),
                ("LD_LIBRARY_PATH", "/x"),
                ("AOE_URL", "u"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
        };
        let clean = sanitize_env(&env);
        assert_eq!(clean.allowlist, ["EXAMPLE_KEY"]);
        assert_eq!(clean.host_only, ["EXAMPLE_KEY"]);
        assert_eq!(
            clean.defaults.keys().collect::<Vec<_>>(),
            ["EXAMPLE_REGION"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn proxy_command_must_resolve_beneath_the_install_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/proxy"), "").unwrap();
        std::fs::write(temp.path().join("outside"), "").unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside"), root.join("bin/link")).unwrap();

        assert!(resolve_beneath(&root, "bin/proxy").is_ok());
        for rel in ["bin/link", "../outside", "/etc/hosts", "bin/missing"] {
            assert!(resolve_beneath(&root, rel).is_err(), "{rel}");
        }
    }
}

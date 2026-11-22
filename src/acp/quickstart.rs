//! The default structured-view setup: what the machine has, what to configure,
//! and what the bundled agent needs to answer a turn.
//!
//! `aoe setup` and the first-run wizard both plan through [`plan`], so a new
//! surface cannot invent a different default. Detection reads the same ACP
//! registry `aoe acp doctor` reports, never a second agent list.

use std::path::Path;

use crate::acp::agent_registry::AgentRegistry;
use crate::acp::install_hints::AOE_AGENT_BINARY;
use crate::session::config::{AcpConfig, StructuredView, DEFAULT_ACP_AGENT};

/// The agent AoE bundles, and the quickstart's default.
pub const BUNDLED_AGENT: &str = AOE_AGENT_BINARY;

pub const OTARI_SIGNUP_URL: &str = "https://otari.ai/dashboard/#/signup";
pub const OTARI_QUICKSTART_URL: &str = "https://docs.otari.ai/getting-started/quickstart";
pub const OTARI_SUPPORT_URL: &str = "https://discord.gg/4gf3zXrQUc";

/// An `AOE_AGENT_MODEL` value that routes the bundled agent through Otari.
pub const OTARI_MODEL_EXAMPLE: &str = "openai-compatible:anthropic/claude-fable-5";

/// The bundled agent is unusable and no install is permitted, so a detected
/// agent takes the default.
const FALLBACK_NO_BUNDLE: &str =
    "the bundled aoe-agent is not installed and installing it was not requested";

/// What this machine has, as `aoe agents` and `aoe acp doctor` see it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    /// Registry agents whose command resolves, in registry order. Includes the
    /// bundled agent once its adapter is installed.
    pub present_agents: Vec<String>,
    /// The bundled `aoe-agent` adapter is installed and current.
    pub bundled_installed: bool,
    /// An OpenAI-compatible gateway credential is already in the environment.
    pub gateway_credential: Option<GatewayCredential>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayCredential {
    /// `OTARI_API_KEY`.
    Otari,
    /// `OPENAI_API_KEY` plus `OPENAI_BASE_URL`: a gateway the user already has.
    Gateway,
}

impl Detection {
    /// Read the host as the doctor does. `app_dir` is only used for the bundled
    /// adapter's install state.
    pub fn detect(app_dir: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let registry = AgentRegistry::with_defaults();
        let mut present_agents: Vec<String> = registry
            .list()
            .into_iter()
            .filter(|(_, spec)| crate::cli::acp::command_present(&spec.command))
            .map(|(name, _)| name.clone())
            .collect();
        present_agents.sort();
        let gateway_credential = if non_empty(env, "OTARI_API_KEY") {
            Some(GatewayCredential::Otari)
        } else if non_empty(env, "OPENAI_API_KEY") && non_empty(env, "OPENAI_BASE_URL") {
            Some(GatewayCredential::Gateway)
        } else {
            None
        };
        let bundled_installed = app_dir
            .is_some_and(|app_dir| crate::acp::adapters::runtime_is_ready(app_dir, BUNDLED_AGENT));
        Self {
            present_agents,
            bundled_installed,
            gateway_credential,
        }
    }

    /// [`Detection::detect`] against the real environment and app dir.
    pub fn detect_env() -> Self {
        let app_dir = crate::session::get_app_dir().ok();
        Self::detect(app_dir.as_deref(), &|name| std::env::var(name).ok())
    }
}

fn non_empty(env: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    env(name).is_some_and(|value| !value.trim().is_empty())
}

/// Whether the bundled agent can start a turn here, by the rule launch applies:
/// the adapter is installed and its Node runtime can run the sources.
pub fn bundled_ready(app_dir: Option<&Path>) -> bool {
    app_dir.is_some_and(|app_dir| crate::acp::adapters::runtime_is_ready(app_dir, BUNDLED_AGENT))
}

/// What the quickstart will do, once detection is folded in with the user's
/// existing configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The agent `acp.default_agent` should name.
    pub agent: String,
    /// The plan's agent is the bundled one.
    pub bundled: bool,
    /// The bundled adapter is installed and current.
    pub bundled_installed: bool,
    /// Bundled adapter to install, when it is missing and allowed.
    pub install: Option<&'static str>,
    /// Agents already on the machine, offered as the alternative to the
    /// bundled default.
    pub alternatives: Vec<String>,
    /// Explain how to get an Otari key: the bundled agent has no credential.
    pub needs_gateway_key: bool,
    /// The gateway credential detection found, which decides the model.
    pub gateway_credential: Option<GatewayCredential>,
    /// Why the bundled agent did not take the default.
    pub fallback: Option<&'static str>,
}

/// Decide the default from detection plus the current config. An existing
/// value the user chose wins over everything: the quickstart never overwrites
/// it.
pub fn plan(detection: &Detection, config: &AcpConfig, install_allowed: bool) -> Plan {
    let alternatives: Vec<String> = detection
        .present_agents
        .iter()
        .filter(|name| name.as_str() != BUNDLED_AGENT && name.as_str() != "claude-code")
        .cloned()
        .collect();

    let configured = config.default_agent.trim();
    let user_default =
        (!configured.is_empty() && configured != DEFAULT_ACP_AGENT).then(|| configured.to_string());

    let (agent, bundled, fallback) = match user_default {
        Some(existing) => {
            let bundled = existing == BUNDLED_AGENT;
            (existing, bundled, None)
        }
        None if detection.bundled_installed || install_allowed => {
            (BUNDLED_AGENT.to_string(), true, None)
        }
        None => match alternatives.first() {
            Some(first) => (first.clone(), false, Some(FALLBACK_NO_BUNDLE)),
            // Nothing is present and nothing may be installed: still name the
            // bundled default, so the doctor command the caller prints applies.
            None => (BUNDLED_AGENT.to_string(), true, Some(FALLBACK_NO_BUNDLE)),
        },
    };

    let install = (agent == BUNDLED_AGENT && !detection.bundled_installed && install_allowed)
        .then_some(BUNDLED_AGENT);

    Plan {
        agent,
        bundled,
        bundled_installed: detection.bundled_installed,
        install,
        alternatives,
        needs_gateway_key: detection.gateway_credential.is_none(),
        gateway_credential: detection.gateway_credential,
        fallback,
    }
}

/// The model the bundled agent has to run on here. With only the Otari key the
/// adapter's own default is an Anthropic id, which needs an Anthropic
/// credential, so the plan names an Otari model instead. A detected
/// `OPENAI_BASE_URL` gateway is left alone: its model ids are the user's.
pub fn gateway_model(plan: &Plan) -> Option<&'static str> {
    (plan.bundled && plan.gateway_credential == Some(GatewayCredential::Otari))
        .then_some(OTARI_MODEL_EXAMPLE)
}

/// The settings a plan has to write, given what the config already says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// `acp.default_agent`, when it is still the built-in default.
    pub default_agent: Option<String>,
    /// `acp.structured_view`, when it is still `auto`.
    pub structured_view: bool,
    /// `acp.acp_defaults.<bundled>.model`, while it has no model.
    pub model: Option<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.default_agent.is_none() && !self.structured_view && self.model.is_none()
    }
}

/// Settings to write for `plan`. Only a value that is still the built-in
/// default is written, so a user-set value and a concurrent write are both left
/// alone. `explicit` is an `--agent` request, which replaces whatever is there:
/// that is the user asking for this default, not a plan guessing at one.
pub fn changes(config: &AcpConfig, plan: &Plan, explicit: bool) -> Changes {
    let configured = config.default_agent.trim();
    let still_default = configured.is_empty() || configured == DEFAULT_ACP_AGENT;
    let has_model = config
        .acp_defaults
        .get(BUNDLED_AGENT)
        .and_then(|defaults| defaults.model.as_deref())
        .is_some_and(|model| !model.trim().is_empty());
    Changes {
        default_agent: ((still_default || explicit) && configured != plan.agent)
            .then(|| plan.agent.clone()),
        structured_view: config.structured_view == StructuredView::Auto,
        model: (!has_model)
            .then(|| gateway_model(plan).map(str::to_string))
            .flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(present: &[&str], bundled_installed: bool, credential: bool) -> Detection {
        Detection {
            present_agents: present.iter().map(|s| s.to_string()).collect(),
            bundled_installed,
            gateway_credential: credential.then_some(GatewayCredential::Otari),
        }
    }

    fn config(default_agent: &str, structured_view: StructuredView) -> AcpConfig {
        AcpConfig {
            default_agent: default_agent.to_string(),
            structured_view,
            ..AcpConfig::default()
        }
    }

    #[test]
    fn the_bundled_agent_is_the_default_in_every_unconfigured_case() {
        let cases: [(&str, Detection, bool); 3] = [
            (
                "fresh machine, install allowed",
                detection(&[], false, false),
                true,
            ),
            (
                "bundled adapter already installed",
                detection(&["aoe-agent", "claude"], true, true),
                false,
            ),
            (
                "other agents present, install still allowed",
                detection(&["claude", "codex"], false, false),
                true,
            ),
        ];
        for (name, detection, install_allowed) in cases {
            let config = config(DEFAULT_ACP_AGENT, StructuredView::Auto);
            let plan = plan(&detection, &config, install_allowed);
            assert_eq!(plan.agent, BUNDLED_AGENT, "{name}");
            assert!(plan.bundled, "{name}");
            assert_eq!(plan.fallback, None, "{name}");
            assert_eq!(
                plan.install,
                (!detection.bundled_installed && install_allowed).then_some(BUNDLED_AGENT),
                "{name}"
            );
            // The bundled agent is offered alongside, never as an alternative.
            assert!(
                !plan.alternatives.iter().any(|a| a == BUNDLED_AGENT),
                "{name}"
            );
        }
    }

    #[test]
    fn a_detected_agent_wins_only_when_the_bundled_one_cannot_run() {
        let config = config(DEFAULT_ACP_AGENT, StructuredView::Auto);

        let fallback = plan(
            &detection(&["claude", "codex"], false, true),
            &config,
            false,
        );
        assert_eq!(fallback.agent, "claude");
        assert!(!fallback.bundled);
        assert_eq!(fallback.install, None);
        assert_eq!(fallback.fallback, Some(FALLBACK_NO_BUNDLE));
        assert_eq!(fallback.alternatives, vec!["claude", "codex"]);

        // Nothing present and nothing installable: still name the bundled one,
        // so the doctor command printed with it is the fix.
        let nothing = plan(&detection(&[], false, true), &config, false);
        assert_eq!(nothing.agent, BUNDLED_AGENT);
        assert!(nothing.bundled);
        assert_eq!(nothing.install, None);
    }

    #[test]
    fn an_existing_default_is_never_overwritten() {
        let cases = ["codex", "claude", BUNDLED_AGENT];
        for existing in cases {
            let config = config(existing, StructuredView::Auto);
            let detection = detection(&["aoe-agent", "claude", "codex"], true, true);
            let plan = plan(&detection, &config, true);
            assert_eq!(plan.agent, existing);
            assert_eq!(plan.bundled, existing == BUNDLED_AGENT);
            assert_eq!(
                changes(&config, &plan, false).default_agent,
                None,
                "{existing}"
            );
            // A detected agent that is chosen is never installed over.
            assert_eq!(plan.install, None, "{existing}");
        }
    }

    #[test]
    fn writes_happen_only_where_the_config_is_still_default() {
        let plan = plan(
            &detection(&[], false, false),
            &config("", StructuredView::Auto),
            true,
        );

        let fresh = changes(&config("", StructuredView::Auto), &plan, false);
        assert_eq!(fresh.default_agent.as_deref(), Some(BUNDLED_AGENT));
        assert!(fresh.structured_view);
        assert!(!fresh.is_empty());

        // Second run: nothing left to write.
        let settled = config(BUNDLED_AGENT, StructuredView::Structured);
        assert!(changes(&settled, &plan, false).is_empty());

        // The user's view choice is theirs, whatever it is.
        for view in [
            StructuredView::Terminal,
            StructuredView::Off,
            StructuredView::Structured,
        ] {
            let chosen = config(DEFAULT_ACP_AGENT, view);
            assert!(!changes(&chosen, &plan, false).structured_view, "{view:?}");
        }
    }

    #[test]
    fn a_default_written_after_planning_is_not_replaced() {
        let plan = plan(
            &detection(&[], false, false),
            &config(DEFAULT_ACP_AGENT, StructuredView::Auto),
            true,
        );
        // Another writer set a default while the install ran: the plan is stale.
        let raced = config("codex", StructuredView::Auto);
        assert_eq!(changes(&raced, &plan, false).default_agent, None);

        // An `--agent` request replaces even a non-default value: that is the
        // user asking for it, and setup reported it as the default.
        assert_eq!(
            changes(&raced, &plan, true).default_agent.as_deref(),
            Some(BUNDLED_AGENT)
        );
    }

    #[test]
    fn only_the_otari_credential_supplies_a_model() {
        let configured = config("", StructuredView::Auto);
        let otari = plan(&detection(&[], true, true), &configured, false);
        // Without this the adapter's own default is an Anthropic id, which the
        // Otari key cannot authenticate.
        assert_eq!(
            changes(&configured, &otari, false).model.as_deref(),
            Some(OTARI_MODEL_EXAMPLE)
        );

        // A model already set for the bundled agent is kept.
        let mut pinned = config("", StructuredView::Auto);
        pinned.acp_defaults.insert(
            BUNDLED_AGENT.to_string(),
            crate::session::config::AcpAgentDefaults {
                model: Some("openai-compatible:moonshotai/kimi-k3.1-pro".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(changes(&pinned, &otari, false).model, None);

        // A detected OpenAI-compatible gateway has its own model ids.
        let gateway = Detection {
            gateway_credential: Some(GatewayCredential::Gateway),
            bundled_installed: true,
            ..Detection::default()
        };
        let gateway_plan = plan(&gateway, &configured, false);
        assert_eq!(changes(&configured, &gateway_plan, false).model, None);

        // No credential: the guidance asks for a key instead of pinning a model.
        let credentialless = plan(&detection(&[], true, false), &configured, true);
        assert_eq!(changes(&configured, &credentialless, false).model, None);
    }

    #[test]
    fn the_otari_guidance_tracks_the_gateway_credential() {
        let config = config(DEFAULT_ACP_AGENT, StructuredView::Auto);
        assert!(
            plan(&detection(&[], true, false), &config, true).needs_gateway_key,
            "no credential means the account guidance applies"
        );
        assert!(!plan(&detection(&[], true, true), &config, true).needs_gateway_key);

        // A gateway credential already in the environment counts too.
        let env = |name: &str| match name {
            "OPENAI_API_KEY" => Some("sk-test".to_string()),
            "OPENAI_BASE_URL" => Some("https://gateway.example/v1".to_string()),
            _ => None,
        };
        assert_eq!(
            Detection::detect(None, &env).gateway_credential,
            Some(GatewayCredential::Gateway)
        );
        let otari_env = |name: &str| (name == "OTARI_API_KEY").then(|| "key".to_string());
        assert_eq!(
            Detection::detect(None, &otari_env).gateway_credential,
            Some(GatewayCredential::Otari)
        );
        // A blank value is not a credential.
        let blank_env = |name: &str| (name == "OTARI_API_KEY").then(|| "  ".to_string());
        assert_eq!(Detection::detect(None, &blank_env).gateway_credential, None);
        // An `OTARI_BASE_URL` with no key beside a complete OpenAI pair is that
        // pair, which is the rule the adapter reads too.
        let mixed_env = |name: &str| match name {
            "OTARI_BASE_URL" => Some("https://otari.example/v1".to_string()),
            "OPENAI_API_KEY" | "OPENAI_BASE_URL" => Some("set".to_string()),
            _ => None,
        };
        assert_eq!(
            Detection::detect(None, &mixed_env).gateway_credential,
            Some(GatewayCredential::Gateway)
        );
    }
}

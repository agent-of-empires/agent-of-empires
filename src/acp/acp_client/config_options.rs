//! Session config options and modes: the ACP wire mapping and the
//! dispatch that applies a requested value.

use crate::acp::state::{ConfigOptionCategory, ConfigOptionChoice, ConfigOptionDescriptor, Event};
use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigValueId, SessionId, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Build a `ConfigOptionsUpdated` event from a session response's
/// `config_options`, or `None` when the response carried none (so the
/// structured view's cached selectors persist). A present-but-empty list is a
/// real full replacement and must propagate, otherwise stale selectors
/// never clear when an adapter intentionally drops them (see #1403).
///
/// Model selection rides the generic `config_option` channel (category
/// `Model`, config id `model`): claude-agent-acp >=0.44 and the ACP
/// crate >=0.14 dropped the dedicated `session/set_model` capability in
/// favor of session config options, so there is no longer a second
/// channel to normalize. See #1403, #1820.
pub(super) fn config_options_event(
    raw: Option<Vec<agent_client_protocol::schema::v1::SessionConfigOption>>,
) -> Option<Event> {
    raw.map(|raw| Event::ConfigOptionsUpdated {
        options: raw.into_iter().filter_map(map_acp_config_option).collect(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigOptionDispatchPurpose {
    Generic,
    Mode,
}

pub(super) fn config_option_success_events(
    options: Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
    value: String,
    purpose: ConfigOptionDispatchPurpose,
) -> Vec<Event> {
    let mut events = config_options_event(Some(options))
        .into_iter()
        .collect::<Vec<_>>();
    if purpose == ConfigOptionDispatchPurpose::Mode {
        events.push(Event::CurrentModeChanged {
            current_mode_id: value,
        });
    }
    events
}

pub(super) fn config_option_failure_event(
    config_id: String,
    value: String,
    reason: String,
    purpose: ConfigOptionDispatchPurpose,
) -> Event {
    match purpose {
        ConfigOptionDispatchPurpose::Generic => Event::ConfigOptionSwitchFailed {
            config_id,
            value,
            reason,
        },
        ConfigOptionDispatchPurpose::Mode => Event::ModeSwitchFailed {
            mode_id: value,
            reason,
        },
    }
}

/// Route a config-option change to `session/set_config_option` and emit
/// the resulting UI update. claude-agent-acp returns the full
/// updated config_options list in the response but does NOT emit a
/// follow-up `config_option_update` notification (see
/// acp-agent.js:1358-1410), so the success path re-emits a
/// `ConfigOptionsUpdated` snapshot from the response and the frontend
/// reducer clears pending state. Mode changes also preserve the semantic
/// mode events consumed by the native structured view. The round-trip is
/// spawned detached so the command loop never blocks on it. See #1403.
pub(super) fn dispatch_set_config_option(
    connection: &ConnectionTo<Agent>,
    acp_session_id: &SessionId,
    config_id: String,
    value: String,
    purpose: ConfigOptionDispatchPurpose,
    event_tx: mpsc::Sender<Event>,
) {
    info!(
        target: "acp.protocol",
        "sending session/set_config_option {config_id}={value}"
    );
    let sent = connection.send_request(SetSessionConfigOptionRequest::new(
        acp_session_id.clone(),
        SessionConfigId::new(config_id.clone()),
        SessionConfigValueId::new(value.clone()),
    ));
    tokio::spawn(async move {
        match sent.block_task().await {
            Ok(resp) => {
                for event in config_option_success_events(resp.config_options, value, purpose) {
                    let _ = event_tx.send(event).await;
                }
            }
            Err(e) => {
                let reason = format!("{e}");
                warn!(
                    target: "acp.protocol",
                    "session/set_config_option failed: {reason}"
                );
                let event = config_option_failure_event(config_id, value, reason, purpose);
                let _ = event_tx.send(event).await;
            }
        }
    });
}

pub(super) fn thought_level_config_id(
    options: &[agent_client_protocol::schema::v1::SessionConfigOption],
) -> Option<agent_client_protocol::schema::v1::SessionConfigId> {
    use agent_client_protocol::schema::v1::{SessionConfigKind, SessionConfigOptionCategory};

    options.iter().find_map(|option| {
        if !matches!(
            option.category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        ) {
            return None;
        }
        if !matches!(option.kind, SessionConfigKind::Select(_)) {
            return None;
        }
        Some(option.id.clone())
    })
}

/// Id of the first `Select` config option in the `mode` category, or `None`.
/// Mirrors `thought_level_config_id`; non-`Select` kinds are skipped because
/// they carry no selectable value the default-application path can set.
pub(super) fn mode_config_id(
    options: &[agent_client_protocol::schema::v1::SessionConfigOption],
) -> Option<agent_client_protocol::schema::v1::SessionConfigId> {
    use agent_client_protocol::schema::v1::{SessionConfigKind, SessionConfigOptionCategory};

    options.iter().find_map(|option| {
        if !matches!(option.category, Some(SessionConfigOptionCategory::Mode)) {
            return None;
        }
        if !matches!(option.kind, SessionConfigKind::Select(_)) {
            return None;
        }
        Some(option.id.clone())
    })
}

/// Build a structured view `ConfigOptionDescriptor` from an ACP
/// `SessionConfigOption`. Returns `None` when the option has a kind
/// the structured view does not yet render (today everything except `Select`).
/// See #1403.
pub(super) fn map_acp_config_option(
    option: agent_client_protocol::schema::v1::SessionConfigOption,
) -> Option<ConfigOptionDescriptor> {
    use agent_client_protocol::schema::v1::{
        SessionConfigKind, SessionConfigOptionCategory, SessionConfigSelectOptions,
    };

    let category = option.category.map(|c| match c {
        SessionConfigOptionCategory::Mode => ConfigOptionCategory::Mode,
        SessionConfigOptionCategory::Model => ConfigOptionCategory::Model,
        SessionConfigOptionCategory::ThoughtLevel => ConfigOptionCategory::ThoughtLevel,
        // "Model-related configuration parameter", a sibling of `Model`
        // rather than another model picker (an adapter ships both), so it
        // must not collapse into `ConfigOptionCategory::Model`. Nothing
        // renders it specially yet, so it carries its upstream wire name
        // through the generic `Other` arm instead of the `Other("")` the
        // catch-all below used to produce (#3403).
        SessionConfigOptionCategory::ModelConfig => {
            ConfigOptionCategory::Other("model_config".to_string())
        }
        SessionConfigOptionCategory::Other(s) => ConfigOptionCategory::Other(s),
        // The schema enum is `#[non_exhaustive]`, so this arm is required
        // to compile. Unknown category *names* arrive via the untagged
        // `Other(String)` arm above; this fires only when upstream adds a
        // genuinely new named variant we haven't mapped yet. Warn so the
        // gap is visible instead of silently surfacing a categoryless
        // option with an empty payload.
        other => {
            tracing::warn!(
                target: "acp.protocol",
                variant = ?other,
                "unknown SessionConfigOptionCategory; treating as Other(\"\"). \
                 Bump claude-agent-acp or add a match arm.",
            );
            ConfigOptionCategory::Other(String::new())
        }
    });

    // Only `Select` is rendered today; future kinds (boolean toggles
    // behind `unstable_boolean_config`) skip until the structured view grows a
    // matching widget. The schema enum is `#[non_exhaustive]` so a
    // catch-all is required.
    let select = match option.kind {
        SessionConfigKind::Select(s) => s,
        _ => return None,
    };

    let choices: Vec<ConfigOptionChoice> = match select.options {
        SessionConfigSelectOptions::Ungrouped(opts) => opts
            .into_iter()
            .map(|o| ConfigOptionChoice {
                value: o.value.0.to_string(),
                name: o.name,
                description: o.description,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .into_iter()
            .flat_map(|g| {
                g.options.into_iter().map(|o| ConfigOptionChoice {
                    value: o.value.0.to_string(),
                    name: o.name,
                    description: o.description,
                })
            })
            .collect(),
        // Catch-all for `#[non_exhaustive]` future variants.
        _ => Vec::new(),
    };

    Some(ConfigOptionDescriptor {
        id: option.id.0.to_string(),
        name: option.name,
        description: option.description,
        category: category.unwrap_or(ConfigOptionCategory::Other(String::new())),
        current_value: select.current_value.0.to_string(),
        options: choices,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModeSetTarget<'a> {
    ConfigOption(&'a str),
    SessionMode,
}

/// Pick the protocol method for a requested mode. Config-option modes
/// take precedence because adapters that advertise both channels report
/// their authoritative current value through config options. With no mode
/// metadata, retain the legacy set_mode fallback for older adapters.
pub(super) fn resolve_mode_set_target<'a>(
    mode_id: &str,
    available_mode_ids: &Option<Vec<String>>,
    mode_config_option_id: Option<&'a str>,
) -> Option<ModeSetTarget<'a>> {
    if let Some(config_id) = mode_config_option_id {
        return Some(ModeSetTarget::ConfigOption(config_id));
    }

    match available_mode_ids {
        Some(ids) => {
            let normalized = mode_id.replace('_', "").to_lowercase();
            ids.iter()
                .any(|id| id.replace('_', "").to_lowercase() == normalized)
                .then_some(ModeSetTarget::SessionMode)
        }
        None => Some(ModeSetTarget::SessionMode),
    }
}

pub(super) fn dispatch_set_mode(
    connection: &ConnectionTo<Agent>,
    acp_session_id: &SessionId,
    mode_id: String,
    available_mode_ids: &Option<Vec<String>>,
    mode_config_option_id: Option<&str>,
    event_tx: mpsc::Sender<Event>,
    while_prompting: bool,
) {
    let Some(target) = resolve_mode_set_target(&mode_id, available_mode_ids, mode_config_option_id)
    else {
        debug!(
            target: "acp.protocol",
            "skipping mode switch mode={mode_id}: not advertised"
        );
        return;
    };

    if let ModeSetTarget::ConfigOption(config_id) = target {
        dispatch_set_config_option(
            connection,
            acp_session_id,
            config_id.to_string(),
            mode_id,
            ConfigOptionDispatchPurpose::Mode,
            event_tx,
        );
        return;
    }

    if while_prompting {
        info!(
            target: "acp.protocol",
            "sending session/set_mode mode={mode_id} during in-flight prompt"
        );
    } else {
        info!(target: "acp.protocol", "sending session/set_mode mode={mode_id}");
    }
    let sent = connection.send_request(SetSessionModeRequest::new(
        acp_session_id.clone(),
        mode_id.clone(),
    ));
    tokio::spawn(async move {
        match sent.block_task().await {
            Ok(_) => {
                let _ = event_tx
                    .send(Event::CurrentModeChanged {
                        current_mode_id: mode_id,
                    })
                    .await;
            }
            Err(e) => {
                let reason = format!("{e}");
                if while_prompting {
                    warn!(
                        target: "acp.protocol",
                        "session/set_mode failed mid-turn: {reason}"
                    );
                } else {
                    warn!(target: "acp.protocol", "session/set_mode failed: {reason}");
                }
                let _ = event_tx
                    .send(Event::ModeSwitchFailed { mode_id, reason })
                    .await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    /// `ModelConfig` is a category upstream already names, so it must map
    /// through the explicit arm: the catch-all's "bump claude-agent-acp"
    /// warning is for variants this build has genuinely never seen, and it
    /// discards the category name on the way past (#3403).
    #[traced_test]
    #[test]
    fn model_config_category_maps_without_the_unknown_variant_warning() {
        use agent_client_protocol::schema::v1::{
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        };
        tracing::callsite::rebuild_interest_cache();
        let mapped = map_acp_config_option(
            SessionConfigOption::select(
                "reasoning-effort",
                "Reasoning effort",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ModelConfig),
        )
        .expect("a select option maps");
        assert_eq!(
            mapped.category,
            ConfigOptionCategory::Other("model_config".to_string())
        );
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|l| l.contains("unknown SessionConfigOptionCategory"))
                .count()
            {
                0 => Ok(()),
                n => Err(format!("expected no unknown-category warning, got {n}")),
            }
        });
    }

    #[test]
    fn resolve_mode_set_target_matches_normalized_session_mode_ids() {
        let ids = Some(vec!["acceptEdits".to_string(), "plan".to_string()]);
        // Underscore + case folding both sides.
        assert_eq!(
            resolve_mode_set_target("accept_edits", &ids, None),
            Some(ModeSetTarget::SessionMode)
        );
        assert_eq!(
            resolve_mode_set_target("acceptEdits", &ids, None),
            Some(ModeSetTarget::SessionMode)
        );
        assert_eq!(
            resolve_mode_set_target("PLAN", &ids, None),
            Some(ModeSetTarget::SessionMode)
        );
        // Not in the advertised set.
        assert_eq!(
            resolve_mode_set_target("bypassPermissions", &ids, None),
            None
        );
    }

    #[test]
    fn profile_yolo_mode_ids_pass_the_advertised_guard() {
        use crate::acp::agent_profiles;
        // Pin each adapter's YOLO id against the modes that adapter actually
        // advertises, so a mismatch (the #1142 codex bug, or the
        // @agentclientprotocol/codex-acp `agent-full-access` rename) can't
        // silently get dropped again.
        let claude_modes = Some(vec![
            "auto".to_string(),
            "default".to_string(),
            "acceptEdits".to_string(),
            "plan".to_string(),
            "bypassPermissions".to_string(),
        ]);
        let codex_modes = Some(vec![
            "read-only".to_string(),
            "agent".to_string(),
            "agent-full-access".to_string(),
        ]);

        let claude_yolo = agent_profiles::resolve("claude").yolo_mode_id.unwrap();
        assert_eq!(
            resolve_mode_set_target(claude_yolo, &claude_modes, None),
            Some(ModeSetTarget::SessionMode)
        );

        let codex_yolo = agent_profiles::resolve("codex").yolo_mode_id.unwrap();
        assert_eq!(
            resolve_mode_set_target(codex_yolo, &codex_modes, None),
            Some(ModeSetTarget::SessionMode)
        );
        // The old hard-coded id would NOT survive the guard for codex.
        assert_eq!(
            resolve_mode_set_target("bypassPermissions", &codex_modes, None),
            None
        );
        // The old Zed adapter id is stale for @agentclientprotocol/codex-acp.
        assert_eq!(
            resolve_mode_set_target("full-access", &codex_modes, None),
            None
        );
    }

    #[test]
    fn config_option_mode_is_authoritative() {
        let ids = Some(
            vec!["agent", "agent-full-access"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        assert_eq!(
            resolve_mode_set_target("agent-full-access", &ids, Some("mode")),
            Some(ModeSetTarget::ConfigOption("mode"))
        );
        assert_eq!(
            resolve_mode_set_target("agent-full-access", &None, Some("mode")),
            Some(ModeSetTarget::ConfigOption("mode"))
        );
        assert_eq!(
            resolve_mode_set_target("plan", &None, None),
            Some(ModeSetTarget::SessionMode)
        );
    }

    #[test]
    fn mode_config_dispatch_preserves_semantic_mode_events() {
        let events = config_option_success_events(
            Vec::new(),
            "agent-full-access".to_string(),
            ConfigOptionDispatchPurpose::Mode,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            Event::ConfigOptionsUpdated { options } if options.is_empty()
        ));
        assert!(matches!(
            &events[1],
            Event::CurrentModeChanged { current_mode_id }
                if current_mode_id == "agent-full-access"
        ));

        let failure = config_option_failure_event(
            "mode".to_string(),
            "agent-full-access".to_string(),
            "rejected".to_string(),
            ConfigOptionDispatchPurpose::Mode,
        );
        assert!(matches!(
            failure,
            Event::ModeSwitchFailed { mode_id, reason }
                if mode_id == "agent-full-access" && reason == "rejected"
        ));

        let generic_failure = config_option_failure_event(
            "model".to_string(),
            "gpt-5".to_string(),
            "rejected".to_string(),
            ConfigOptionDispatchPurpose::Generic,
        );
        assert!(matches!(
            generic_failure,
            Event::ConfigOptionSwitchFailed { config_id, value, reason }
                if config_id == "model" && value == "gpt-5" && reason == "rejected"
        ));
    }

    #[test]
    fn config_options_event_propagates_empty_snapshot() {
        // A present-but-empty config_options snapshot from the adapter is
        // a real full replacement and must clear stale cached selectors,
        // so it returns `Some(ConfigOptionsUpdated { options: [] })`
        // (not `None`). See #1403.
        let event =
            config_options_event(Some(Vec::new())).expect("Some(vec![]) should produce an event");
        match event {
            Event::ConfigOptionsUpdated { options } => {
                assert!(options.is_empty());
            }
            other => panic!("expected empty ConfigOptionsUpdated, got {other:?}"),
        }
        // No config_options field at all (the adapter omitted it) returns
        // None so callers skip the emit and cached selectors persist.
        assert!(config_options_event(None).is_none());
    }
}

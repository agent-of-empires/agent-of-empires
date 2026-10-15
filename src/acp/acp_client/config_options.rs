//! Session config options and modes: the ACP wire mapping and the
//! dispatch that applies a requested value.

use crate::acp::state::{
    ConfigOptionCategory, ConfigOptionChoice, ConfigOptionDescriptor, Event, ModeInfo,
};
use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionConfigValueId, SessionId, SessionModeState,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// `None` when the response carried no options, so cached selectors persist.
/// An empty list is a real replacement and must propagate (#1403).
pub(super) fn config_options_event(raw: Option<Vec<SessionConfigOption>>) -> Option<Event> {
    raw.map(|raw| Event::ConfigOptionsUpdated {
        options: raw.into_iter().filter_map(map_acp_config_option).collect(),
    })
}

pub(super) fn modes_available_event(modes: &SessionModeState) -> Event {
    Event::ModesAvailable {
        current_mode_id: modes.current_mode_id.0.to_string(),
        modes: modes
            .available_modes
            .iter()
            .map(|m| ModeInfo {
                id: m.id.0.to_string(),
                name: m.name.clone(),
                description: m.description.clone(),
            })
            .collect(),
    }
}

/// The mode and effort channels a session response advertised.
#[derive(Debug, Default)]
pub(super) struct SessionChannels {
    pub(super) available_mode_ids: Option<Vec<String>>,
    pub(super) mode_config_option_id: Option<String>,
    pub(super) thought_level_config_option_id: Option<String>,
    pub(super) model_option: Option<ModelOption>,
}

impl SessionChannels {
    pub(super) fn new(
        modes: Option<&SessionModeState>,
        options: Option<&[SessionConfigOption]>,
    ) -> Self {
        Self {
            available_mode_ids: modes.map(|m| {
                m.available_modes
                    .iter()
                    .map(|mode| mode.id.0.to_string())
                    .collect()
            }),
            mode_config_option_id: options.and_then(mode_config_id).map(|id| id.0.to_string()),
            thought_level_config_option_id: options
                .and_then(thought_level_config_id)
                .map(|id| id.0.to_string()),
            model_option: options.and_then(model_option),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigOptionDispatchPurpose {
    Generic,
    Mode,
}

fn config_option_success_events(
    options: Vec<SessionConfigOption>,
    value: String,
    purpose: ConfigOptionDispatchPurpose,
) -> Vec<Event> {
    let mut events: Vec<_> = config_options_event(Some(options)).into_iter().collect();
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

/// The adapter answers with the full option list but sends no follow-up
/// notification, so the response itself is re-emitted. Runs detached so the
/// command loop never blocks.
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
        let events = match sent.block_task().await {
            Ok(resp) => config_option_success_events(resp.config_options, value, purpose),
            Err(e) => {
                let reason = e.to_string();
                warn!(target: "acp.protocol", "session/set_config_option failed: {reason}");
                vec![config_option_failure_event(
                    config_id, value, reason, purpose,
                )]
            }
        };
        for event in events {
            let _ = event_tx.send(event).await;
        }
    });
}

/// Best-effort application of a configured default. A value the agent no
/// longer advertises is rejected and warned, never failing the caller.
/// Returns the agent's option list, or the rejection reason.
pub(super) async fn apply_config_default(
    connection: &ConnectionTo<Agent>,
    event_tx: &mpsc::Sender<Event>,
    session_id: SessionId,
    config_id: SessionConfigId,
    value: &str,
    session_label: &str,
) -> Result<Vec<SessionConfigOption>, String> {
    info!(
        target: "acp.protocol",
        session = %session_label,
        config_id = %config_id.0,
        value,
        "applying structured view default"
    );
    let request = SetSessionConfigOptionRequest::new(
        session_id,
        config_id,
        SessionConfigValueId::new(value.to_string()),
    );
    match connection.send_request(request).block_task().await {
        Ok(resp) => {
            if let Some(event) = config_options_event(Some(resp.config_options.clone())) {
                let _ = event_tx.send(event).await;
            }
            Ok(resp.config_options)
        }
        Err(e) => {
            warn!(
                target: "acp.protocol",
                session = %session_label,
                "structured view default failed: {e}"
            );
            Err(e.to_string())
        }
    }
}

/// First `Select` option in `category`; other kinds have no settable value.
fn select_config_id(
    options: &[SessionConfigOption],
    category: SessionConfigOptionCategory,
) -> Option<SessionConfigId> {
    options
        .iter()
        .find(|o| {
            o.category.as_ref() == Some(&category) && matches!(o.kind, SessionConfigKind::Select(_))
        })
        .map(|o| o.id.clone())
}

pub(super) fn thought_level_config_id(options: &[SessionConfigOption]) -> Option<SessionConfigId> {
    select_config_id(options, SessionConfigOptionCategory::ThoughtLevel)
}

pub(super) fn mode_config_id(options: &[SessionConfigOption]) -> Option<SessionConfigId> {
    select_config_id(options, SessionConfigOptionCategory::Mode)
}

/// The `category:"model"` select a session advertised, as last reported.
#[derive(Debug)]
pub(super) struct ModelOption {
    pub(super) id: String,
    current_value: String,
    current_name: Option<String>,
}

impl ModelOption {
    /// Whether re-sending `pick` would be a no-op. Matching the display name
    /// too spares the round-trip for aliases like `opus` that the agent
    /// reports as a canonical id; fuzzier aliases are still re-sent.
    pub(super) fn is_current(&self, pick: &str) -> bool {
        self.current_value.eq_ignore_ascii_case(pick)
            || self
                .current_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(pick))
    }
}

pub(super) fn model_option(options: &[SessionConfigOption]) -> Option<ModelOption> {
    options.iter().find_map(|o| {
        let SessionConfigKind::Select(select) = &o.kind else {
            return None;
        };
        if o.category != Some(SessionConfigOptionCategory::Model) {
            return None;
        }
        let current = &select.current_value;
        let current_name = match &select.options {
            SessionConfigSelectOptions::Ungrouped(opts) => opts
                .iter()
                .find(|c| &c.value == current)
                .map(|c| c.name.clone()),
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|g| &g.options)
                .find(|c| &c.value == current)
                .map(|c| c.name.clone()),
            _ => None,
        };
        Some(ModelOption {
            id: o.id.0.to_string(),
            current_value: current.0.to_string(),
            current_name,
        })
    })
}

/// `None` for kinds the structured view does not render (all but `Select`).
pub(super) fn map_acp_config_option(option: SessionConfigOption) -> Option<ConfigOptionDescriptor> {
    let category = option.category.map(|c| match c {
        SessionConfigOptionCategory::Mode => ConfigOptionCategory::Mode,
        SessionConfigOptionCategory::Model => ConfigOptionCategory::Model,
        SessionConfigOptionCategory::ThoughtLevel => ConfigOptionCategory::ThoughtLevel,
        // A sibling of `Model`, not another model picker (#3403).
        SessionConfigOptionCategory::ModelConfig => {
            ConfigOptionCategory::Other("model_config".to_string())
        }
        SessionConfigOptionCategory::Other(s) => ConfigOptionCategory::Other(s),
        // Only a genuinely new named upstream variant reaches this arm.
        other => {
            warn!(
                target: "acp.protocol",
                variant = ?other,
                "unknown SessionConfigOptionCategory; treating as Other(\"\"). \
                 Bump claude-agent-acp or add a match arm.",
            );
            ConfigOptionCategory::Other(String::new())
        }
    });
    let SessionConfigKind::Select(select) = option.kind else {
        return None;
    };
    let choice =
        |o: agent_client_protocol::schema::v1::SessionConfigSelectOption| ConfigOptionChoice {
            value: o.value.0.to_string(),
            name: o.name,
            description: o.description,
        };
    let options = match select.options {
        SessionConfigSelectOptions::Ungrouped(opts) => opts.into_iter().map(choice).collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .into_iter()
            .flat_map(|g| g.options.into_iter().map(choice))
            .collect(),
        _ => Vec::new(),
    };
    Some(ConfigOptionDescriptor {
        id: option.id.0.to_string(),
        name: option.name,
        description: option.description,
        category: category.unwrap_or(ConfigOptionCategory::Other(String::new())),
        current_value: select.current_value.0.to_string(),
        options,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeSetTarget<'a> {
    ConfigOption(&'a str),
    SessionMode,
}

/// Config-option modes are authoritative when advertised. Without any mode
/// metadata, fall back to legacy `session/set_mode`.
fn resolve_mode_set_target<'a>(
    mode_id: &str,
    available_mode_ids: &Option<Vec<String>>,
    mode_config_option_id: Option<&'a str>,
) -> Option<ModeSetTarget<'a>> {
    if let Some(config_id) = mode_config_option_id {
        return Some(ModeSetTarget::ConfigOption(config_id));
    }
    let Some(ids) = available_mode_ids else {
        return Some(ModeSetTarget::SessionMode);
    };
    let normalize = |id: &str| id.replace('_', "").to_lowercase();
    let normalized = normalize(mode_id);
    ids.iter()
        .any(|id| normalize(id) == normalized)
        .then_some(ModeSetTarget::SessionMode)
}

pub(super) fn dispatch_set_mode(
    connection: &ConnectionTo<Agent>,
    acp_session_id: &SessionId,
    mode_id: String,
    channels: &SessionChannels,
    event_tx: mpsc::Sender<Event>,
    while_prompting: bool,
) {
    let target = resolve_mode_set_target(
        &mode_id,
        &channels.available_mode_ids,
        channels.mode_config_option_id.as_deref(),
    );
    let Some(target) = target else {
        debug!(target: "acp.protocol", "skipping mode switch mode={mode_id}: not advertised");
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
    info!(
        target: "acp.protocol",
        while_prompting,
        "sending session/set_mode mode={mode_id}"
    );
    let sent = connection.send_request(SetSessionModeRequest::new(
        acp_session_id.clone(),
        mode_id.clone(),
    ));
    tokio::spawn(async move {
        let event = match sent.block_task().await {
            Ok(_) => Event::CurrentModeChanged {
                current_mode_id: mode_id,
            },
            Err(e) => {
                let reason = e.to_string();
                warn!(target: "acp.protocol", while_prompting, "session/set_mode failed: {reason}");
                Event::ModeSwitchFailed { mode_id, reason }
            }
        };
        let _ = event_tx.send(event).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3403: `ModelConfig` maps explicitly, without the unknown-variant warning.
    #[test]
    fn model_config_options() {
        use agent_client_protocol::schema::v1::SessionConfigSelectOption;
        let logs = crate::session::test_support::LogCapture::start();
        let mapped = map_acp_config_option(
            SessionConfigOption::select(
                "reasoning-effort",
                "Reasoning effort",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ModelConfig),
        )
        .unwrap();
        assert_eq!(
            mapped.category,
            ConfigOptionCategory::Other("model_config".to_string())
        );
        assert!(!logs
            .contents()
            .contains("unknown SessionConfigOptionCategory"));

        {
            use agent_client_protocol::schema::v1::SessionConfigSelectOption;
            let options = [SessionConfigOption::select(
                "model",
                "Model",
                "claude-opus-5",
                vec![
                    SessionConfigSelectOption::new("claude-opus-5", "Opus"),
                    SessionConfigSelectOption::new("claude-sonnet-5", "Sonnet"),
                ],
            )
            .category(SessionConfigOptionCategory::Model)];
            let option = model_option(&options).unwrap();
            assert_eq!(option.id, "model");
            for (pick, want) in [
                ("claude-opus-5", true),
                ("opus", true),
                ("sonnet", false),
                ("claude-sonnet-5", false),
            ] {
                assert_eq!(option.is_current(pick), want, "{pick}");
            }
        }
    }

    #[test]
    fn resolve_mode_set_target_cases() {
        use crate::acp::agent_profiles;
        let ids = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let claude = ids(&[
            "auto",
            "default",
            "acceptEdits",
            "plan",
            "bypassPermissions",
        ]);
        let codex = ids(&["read-only", "agent", "agent-full-access"]);
        let session = Some(ModeSetTarget::SessionMode);
        let claude_yolo = agent_profiles::resolve("claude").yolo_mode_id.unwrap();
        let codex_yolo = agent_profiles::resolve("codex").yolo_mode_id.unwrap();
        let cases = [
            // Underscore and case folding on both sides.
            ("accept_edits", &claude, None, session),
            ("PLAN", &claude, None, session),
            (
                "bypassPermissions",
                &ids(&["acceptEdits", "plan"]),
                None,
                None,
            ),
            // #1142: each profile's YOLO id must pass its adapter's guard.
            (claude_yolo, &claude, None, session),
            (codex_yolo, &codex, None, session),
            ("bypassPermissions", &codex, None, None),
            ("full-access", &codex, None, None),
            (
                "agent-full-access",
                &codex,
                Some("mode"),
                Some(ModeSetTarget::ConfigOption("mode")),
            ),
            (
                "agent-full-access",
                &None,
                Some("mode"),
                Some(ModeSetTarget::ConfigOption("mode")),
            ),
            ("plan", &None, None, session),
        ];
        for (mode, available, config_id, want) in cases {
            assert_eq!(
                resolve_mode_set_target(mode, available, config_id),
                want,
                "{mode}"
            );
        }
    }
}

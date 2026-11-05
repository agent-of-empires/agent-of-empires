//! Session config options and modes: the ACP wire mapping and the
//! dispatch that applies a requested value.

use crate::acp::establishment::NativeRequestContext;
use crate::acp::event_sender::EventSender;
use crate::acp::protocol::AcpOriginalRequestResult;
use crate::acp::state::{
    ConfigOptionCategory, ConfigOptionChoice, ConfigOptionDescriptor, Event, ModeInfo,
};
use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionConfigValueId, SessionModeState,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use tracing::{debug, info, warn};

use super::connection::ConnectionState;
use super::errors::{acp_internal_error, public_error_message};
use super::session_identity::{
    ordered_native_request, prepare_native_request, OrderedRequestResult,
};
use std::sync::Arc;

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

/// The adapter's response supplies the option list. SDK-owned application
/// custody lasts through the ordered result and publication of its events.
pub(super) fn dispatch_set_config_option(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    request: SetSessionConfigOptionRequest,
    purpose: ConfigOptionDispatchPurpose,
    event_tx: EventSender,
    context: Option<NativeRequestContext>,
) {
    let config_id = request.config_id.0.to_string();
    let value = request
        .value
        .as_value_id()
        .expect("select config request")
        .0
        .to_string();
    info!(
        target: "acp.protocol",
        "sending session/set_config_option {config_id}={value}"
    );
    let native = state.control.get().is_some();
    let sender = if native {
        event_tx.with_context(context.clone().map(Arc::new))
    } else {
        event_tx
    };
    let request = state
        .admit(|| prepare_native_request(connection, state, context, request))
        .and_then(|request| request);
    let refused = request.is_err();
    let publish = async move {
        let (request, _outcome) = match request {
            Ok((request, outcome)) => (Ok(request), Some(outcome)),
            Err(error) => (Err(error), None),
        };
        let result = match request {
            Ok(request) => request.wait().await,
            Err(error) => {
                OrderedRequestResult::Refused(acp_internal_error(format!("NotIssued: {error}")))
            }
        };
        let (sender, events) = match result {
            OrderedRequestResult::Correlated(Ok(response)) => (
                sender.with_outcome(AcpOriginalRequestResult::Succeeded),
                config_option_success_events(response.config_options, value, purpose),
            ),
            OrderedRequestResult::Correlated(Err(error)) => (
                sender.with_outcome(AcpOriginalRequestResult::Failed),
                vec![config_option_failure_event(
                    config_id,
                    value,
                    public_error_message(&error),
                    purpose,
                )],
            ),
            OrderedRequestResult::Refused(error) => (
                sender.historical(),
                vec![config_option_failure_event(
                    config_id,
                    value,
                    public_error_message(&error),
                    purpose,
                )],
            ),
            OrderedRequestResult::Unavailable(_) | OrderedRequestResult::Abandoned => (
                sender.historical(),
                vec![config_option_failure_event(
                    config_id,
                    value,
                    "Config response was not observed".into(),
                    purpose,
                )],
            ),
        };
        for event in events {
            let _ = sender.send(event).await;
        }
        Ok(())
    };
    if refused {
        tokio::spawn(async move {
            let _ = publish.await;
        });
    } else if let Err(error) = connection.spawn(publish) {
        warn!(target: "acp.protocol", %error, "config response task could not be admitted");
    }
}

/// Best-effort application of a configured default. A value the agent no
/// longer advertises is rejected and warned, never failing the caller.
/// Returns the agent's option list, or the rejection reason.
pub(super) async fn apply_config_default(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    event_tx: &EventSender,
    request: SetSessionConfigOptionRequest,
    session_label: &str,
    context: Option<NativeRequestContext>,
) -> Result<Vec<SessionConfigOption>, String> {
    info!(
        target: "acp.protocol",
        session = %session_label,
        config_id = %request.config_id.0,
        value = ?request.value,
        "applying structured view default"
    );
    let sender = if state.control.get().is_some() {
        event_tx.with_context(context.clone().map(Arc::new))
    } else {
        event_tx.clone()
    };
    let request = ordered_native_request(connection, state, context, request)
        .map_err(|_| "ACP connection unavailable".to_string())?;
    match request.wait().await {
        OrderedRequestResult::Correlated(Ok(resp)) => {
            if let Some(event) = config_options_event(Some(resp.config_options.clone())) {
                let _ = sender.send(event).await;
            }
            Ok(resp.config_options)
        }
        OrderedRequestResult::Correlated(Err(error)) | OrderedRequestResult::Refused(error) => {
            warn!(target: "acp.protocol", session = %session_label, ?error, "structured view default failed");
            Err(public_error_message(&error))
        }
        OrderedRequestResult::Unavailable(_) => Err("ACP connection unavailable".into()),
        OrderedRequestResult::Abandoned => Err("Config response was not observed".into()),
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
    state: &Arc<ConnectionState>,
    request: SetSessionModeRequest,
    channels: &SessionChannels,
    event_tx: EventSender,
    while_prompting: bool,
    context: Option<NativeRequestContext>,
) {
    let mode_id = request.mode_id.0.to_string();
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
            state,
            SetSessionConfigOptionRequest::new(
                request.session_id,
                SessionConfigId::new(config_id.to_string()),
                SessionConfigValueId::new(mode_id),
            ),
            ConfigOptionDispatchPurpose::Mode,
            event_tx,
            context,
        );
        return;
    }
    info!(
        target: "acp.protocol",
        while_prompting,
        "sending session/set_mode mode={mode_id}"
    );
    let native = state.control.get().is_some();
    let sender = if native {
        event_tx.with_context(context.clone().map(Arc::new))
    } else {
        event_tx
    };
    let request = state
        .admit(|| prepare_native_request(connection, state, context, request))
        .and_then(|request| request);
    let refused = request.is_err();
    let publish = async move {
        let (request, _outcome) = match request {
            Ok((request, outcome)) => (Ok(request), Some(outcome)),
            Err(error) => (Err(error), None),
        };
        let result = match request {
            Ok(request) => request.wait().await,
            Err(error) => {
                OrderedRequestResult::Refused(acp_internal_error(format!("NotIssued: {error}")))
            }
        };
        let (sender, event) = match result {
            OrderedRequestResult::Correlated(Ok(_)) => (
                sender.with_outcome(AcpOriginalRequestResult::Succeeded),
                Event::CurrentModeChanged {
                    current_mode_id: mode_id,
                },
            ),
            OrderedRequestResult::Correlated(Err(error)) => (
                sender.with_outcome(AcpOriginalRequestResult::Failed),
                Event::ModeSwitchFailed {
                    mode_id,
                    reason: public_error_message(&error),
                },
            ),
            OrderedRequestResult::Refused(error) => (
                sender.historical(),
                Event::ModeSwitchFailed {
                    mode_id,
                    reason: public_error_message(&error),
                },
            ),
            OrderedRequestResult::Unavailable(_) | OrderedRequestResult::Abandoned => (
                sender.historical(),
                Event::ModeSwitchFailed {
                    mode_id,
                    reason: "Mode response was not observed".into(),
                },
            ),
        };
        let _ = sender.send(event).await;
        Ok(())
    };
    if refused {
        tokio::spawn(async move {
            let _ = publish.await;
        });
    } else if let Err(error) = connection.spawn(publish) {
        warn!(target: "acp.protocol", %error, "mode response task could not be admitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn pending_config_and_mode_hold_fresh_until_native_result_is_applied() {
        use crate::acp::acp_client::test_helpers::SdkTestOptions;
        use crate::acp::establishment::{
            AcpEstablishment, EstablishmentOrigin, EstablishmentTarget,
        };
        use crate::acp::supervisor::test_support::managed::with_managed;
        use serde_json::json;
        use std::future::Future;
        use std::time::Duration;
        use tokio::sync::oneshot;

        async fn bounded<T>(future: impl Future<Output = T>) -> T {
            tokio::time::timeout(Duration::from_secs(15), future)
                .await
                .expect("actual managed config/Fresh boundary")
        }

        fn durable_row(id: &str) -> crate::session::Instance {
            crate::session::Storage::open_unwatched("test")
                .unwrap()
                .load_instances_complete()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .expect("genuine managed row")
        }

        with_managed(|scope| {
            Box::pin(async move {
                for case in ["config", "mode-config", "mode-legacy"] {
                    let directory = scope.tempdir();
                    let (write_pending, writing) = oneshot::channel();
                    scope.configure_sdk(SdkTestOptions {
                        fresh_write_pending: Some(write_pending),
                        ..Default::default()
                    });
                    let option = |value: &str| {
                        let mut option = json!({
                            "id":"test-option", "name":"Test option", "type":"select",
                            "currentValue":value,
                            "options":[{"value":"old","name":"Old"},{"value":"new","name":"New"}]
                        });
                        if case == "mode-config" {
                            option["category"] = json!("mode");
                        }
                        option
                    };
                    let initial_option = option("old");
                    let applied_option = option("new");
                    let (setter_seen, setter_received) = oneshot::channel();
                    let (fresh_seen, fresh_received) = oneshot::channel();
                    let (mut setter_seen, mut fresh_seen) = (Some(setter_seen), Some(fresh_seen));
                    let mut news = 0;
                    let (config, native) = scope.native_peer(
                        directory.path().into(),
                        "codex",
                        "codex",
                        move |request| {
                            let id = request.get("id")?;
                            match request["method"].as_str().unwrap() {
                                "initialize" => Some(json!({"jsonrpc":"2.0","id":id,"result":{
                                    "protocolVersion":1,"agentCapabilities":{}
                                }})),
                                "session/new" => {
                                    news += 1;
                                    match news {
                                        1 => {
                                            let mut result = json!({"sessionId":"config-old"});
                                            if case == "mode-legacy" {
                                                result["modes"] = json!({
                                                    "currentModeId":"old",
                                                    "availableModes":[
                                                        {"id":"old","name":"Old"},
                                                        {"id":"new","name":"New"}
                                                    ]
                                                });
                                            } else {
                                                result["configOptions"] = json!([initial_option]);
                                            }
                                            Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
                                        }
                                        2 => {
                                            fresh_seen.take().unwrap().send(id.clone()).unwrap();
                                            None
                                        }
                                        _ => panic!("Fresh cannot issue another Native New"),
                                    }
                                }
                                "session/set_config_option" | "session/set_mode" => {
                                    let expected = if case == "mode-legacy" {
                                        "session/set_mode"
                                    } else {
                                        "session/set_config_option"
                                    };
                                    assert_eq!(request["method"], expected);
                                    assert_eq!(request["params"]["sessionId"], "config-old");
                                    setter_seen.take().unwrap().send(id.clone()).unwrap();
                                    None
                                }
                                other => panic!("unexpected Native operation: {other}"),
                            }
                        },
                    );
                    let mut client = scope.spawn(config, case).await.unwrap();
                    bounded(client.ready()).await;
                    let actual = client.actual();
                    let owner = actual.connected_runner().unwrap();
                    let original = actual
                        .establishment_projection()
                        .unwrap()
                        .receipt_snapshot()
                        .unwrap();
                    let predecessor = durable_row(&client.id).acp_establishment;
                    if case == "config" {
                        client
                            .set_config_option("test-option", "new", None)
                            .await
                            .unwrap();
                    } else {
                        client.set_mode("new", None).await.unwrap();
                    }
                    let setter_id = bounded(setter_received).await.unwrap();
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                    let proposal = client.prepare_reset(deadline).await;
                    let (reset_completed, reset_result) = oneshot::channel();
                    let reset = scope.spawn_helper(async move {
                        let result = actual
                            .reset_session("clear", proposal, deadline, deadline)
                            .await;
                        let _ = reset_completed.send(result);
                    });
                    // This signal comes from the actual writer's first Pending poll.
                    // The peer still owns the unanswered setter on this same runner.
                    bounded(writing).await.unwrap();
                    assert_eq!(durable_row(&client.id).acp_establishment, predecessor);
                    assert_eq!(client.actual().connected_runner(), Some(owner));
                    assert_eq!(
                        native
                            .requests()
                            .iter()
                            .filter(|method| *method == "session/new")
                            .count(),
                        1
                    );

                    let result = if case == "mode-legacy" {
                        json!({})
                    } else {
                        json!({"configOptions":[applied_option]})
                    };
                    native
                        .send(json!({"jsonrpc":"2.0","id":setter_id,"result":result}))
                        .await;
                    bounded(async {
                        loop {
                            match client
                                .next_event()
                                .await
                                .expect("actual managed publication")
                            {
                                Event::ConfigOptionsUpdated { options }
                                    if case == "config"
                                        && options.iter().any(|option| {
                                            option.id == "test-option"
                                                && option.current_value == "new"
                                        }) =>
                                {
                                    break
                                }
                                Event::CurrentModeChanged { current_mode_id }
                                    if case != "config" && current_mode_id == "new" =>
                                {
                                    break
                                }
                                Event::ConfigOptionSwitchFailed { reason, .. }
                                | Event::ModeSwitchFailed { reason, .. } => panic!("{reason}"),
                                Event::AcpConnectionUnavailable { .. } => {
                                    panic!("original physical Native owner must remain live")
                                }
                                Event::AgentStartupError { message, .. } => panic!("{message}"),
                                _ => {}
                            }
                        }
                    })
                    .await;
                    // Fresh may issue now, but cannot replace the snapshot until its
                    // actual Native response below. Inspect the applied setter first.
                    let fresh_id = bounded(fresh_received).await.unwrap();
                    let snapshot = client
                        .app_state()
                        .session_service
                        .fold_control_state(&client.id)
                        .await
                        .unwrap();
                    if case == "config" {
                        assert!(snapshot
                            .config_options
                            .iter()
                            .any(|option| option.id == "test-option"
                                && option.current_value == "new"));
                    } else {
                        assert_eq!(snapshot.current_mode_id.as_deref(), Some("new"));
                    }
                    native
                        .send(json!({"jsonrpc":"2.0","id":fresh_id,
                    "result":{"sessionId":"config-fresh"}}))
                        .await;
                    assert!(matches!(bounded(reset_result).await.unwrap().unwrap(),
                    crate::acp::acp_client::ResetSessionOutcome::Reset { new_acp_session_id }
                        if new_acp_session_id == "config-fresh"));
                    bounded(reset).await.unwrap();
                    bounded(client.ready()).await;
                    let row = durable_row(&client.id);
                    let receipt = client
                        .actual()
                        .establishment_projection()
                        .unwrap()
                        .receipt_snapshot()
                        .unwrap();
                    assert_eq!(client.actual().connected_runner(), Some(owner));
                    assert_eq!(receipt.owner, owner);
                    assert_ne!(receipt.attempt.id, original.attempt.id);
                    assert_eq!(receipt.attempt.origin, EstablishmentOrigin::Fresh);
                    assert_eq!(receipt.attempt.target, EstablishmentTarget::New);
                    assert_eq!(row.acp_session_id.as_deref(), Some("config-fresh"));
                    assert!(matches!(row.acp_establishment,
                    Some(AcpEstablishment::Applied { attempt_id })
                        if attempt_id == receipt.attempt.id));
                    let setter = if case == "mode-legacy" {
                        "session/set_mode"
                    } else {
                        "session/set_config_option"
                    };
                    assert_eq!(
                        native.requests(),
                        ["initialize", "session/new", setter, "session/new"]
                    );
                    client.shutdown().await.unwrap();
                }
            })
        })
        .await;
    }

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

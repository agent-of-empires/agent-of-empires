//! Async worker RPC handlers for the session-driving plugin API (#2897):

use std::sync::Arc;

use serde_json::Value;

use aoe_plugin_api::acp::{
    AcpAgentCapability, AcpCapabilitiesResponse, AcpModeCapability, AcpModelCapability,
    AcpThinkingCapability, ApprovalClass, CatalogStatus,
};
use aoe_plugin_api::session::{
    MessageDisposition, MessageSendRequest, MessageSendResponse, SessionsCreateRequest,
    SessionsCreateResponse, TurnSendRequest,
};

use crate::acp::option_catalog::{AgentOptionEntry, OptionCatalog};
use crate::acp::state::ConfigOptionCategory;
use crate::plugin::automation_policy::{
    classify_mode, AutomationPolicy, ModeDecision, MAX_ACTIVE_PLUGIN_SESSIONS,
};
use crate::plugin::host_api::{DispatchError, PluginRpcContext};
use crate::plugin::protocol::codes;
use crate::server::api::sessions::{send_terminal_message, SendKeysError};
use crate::server::session_service::{
    CreateIdempotencyProbe, IdempotencyConflict, SendTurnError, SendTurnRequest, SessionCaller,
    SessionService,
};
use crate::server::session_spawn::StructuredSessionSpec;

const MAX_EXTRA_PROJECT_PATHS: usize = 16;

const CAP_ACP_CAPABILITIES_READ: &str = "acp.capabilities.read";
const CAP_ACP_CAPABILITIES_PROBE: &str = "acp.capabilities.probe";
const CAP_SESSION_CREATE: &str = "session.create";
const CAP_SESSION_PROMPT: &str = "session.prompt";
const CAP_SESSION_UNATTENDED: &str = "session.unattended";
const CAP_SESSION_MESSAGE: &str = "session.message";

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Whole-call bound for `sessions.message.send`: the structured worker wait plus a terminal
/// resume and its pane-ready wait.
const MESSAGE_DEADLINE: std::time::Duration =
    crate::acp::supervisor::WORKER_READY_TIMEOUT.saturating_add(std::time::Duration::from_secs(15));

pub struct SessionRpcDeps {
    pub session_service: Arc<SessionService>,
    pub policy: Arc<AutomationPolicy>,
    pub profile: String,
    /// Terminal keystroke injection is closed in CityHall mode, so plugins may not reach it either.
    pub cityhall_mode: bool,
    /// Publishes the status move a terminal revive makes, as the HTTP send handler does.
    pub status_tx: tokio::sync::broadcast::Sender<crate::server::push::StatusChange>,
}

async fn clear_revival_pending(session_service: &SessionService, id: &str) {
    let mut instances = session_service.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.plugin_revival_pending = false;
    }
}

pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        "acp.capabilities.get"
            | "sessions.message.send"
            | "acp.capabilities.probe"
            | "sessions.create"
            | "sessions.turn.send"
    )
}

pub(crate) fn required_capability(method: &str) -> Option<&'static str> {
    match method {
        "acp.capabilities.get" => Some(CAP_ACP_CAPABILITIES_READ),
        "acp.capabilities.probe" => Some(CAP_ACP_CAPABILITIES_PROBE),
        "sessions.create" => Some(CAP_SESSION_CREATE),
        "sessions.turn.send" => Some(CAP_SESSION_PROMPT),
        "sessions.message.send" => Some(CAP_SESSION_MESSAGE),
        _ => None,
    }
}

pub(crate) async fn dispatch(
    deps: &Arc<SessionRpcDeps>,
    ctx: &PluginRpcContext,
    method: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    match method {
        "acp.capabilities.get" => {
            ctx.require(CAP_ACP_CAPABILITIES_READ)?;
            capabilities_get().await
        }
        "acp.capabilities.probe" => {
            ctx.require(CAP_ACP_CAPABILITIES_PROBE)?;
            capabilities_probe(params).await
        }
        "sessions.create" => {
            ctx.require(CAP_SESSION_CREATE)?;
            sessions_create(deps, ctx, params).await
        }
        "sessions.turn.send" => {
            ctx.require(CAP_SESSION_PROMPT)?;
            sessions_turn_send(deps, ctx, params).await
        }
        "sessions.message.send" => {
            ctx.require(CAP_SESSION_MESSAGE)?;
            sessions_message_send(deps, ctx, params).await
        }
        other => Err(DispatchError::internal(format!(
            "session_api routed unknown method {other:?}"
        ))),
    }
}

async fn capabilities_get() -> Result<Value, DispatchError> {
    let catalog = load_catalog().await;
    let mut ids: Vec<String> = crate::acp::AgentRegistry::with_defaults()
        .list()
        .into_iter()
        .map(|(name, _)| name.clone())
        .collect();
    for name in catalog.agents.keys() {
        if !ids.contains(name) {
            ids.push(name.clone());
        }
    }
    ids.sort();

    let agents = ids
        .into_iter()
        .map(|id| {
            let entry = catalog.agents.get(&id);
            let (catalog_status, catalog_updated_at) = match entry {
                Some(e) => (CatalogStatus::Discovered, Some(e.updated_at.clone())),
                None => (CatalogStatus::Undiscovered, None),
            };
            let mut models: Vec<AcpModelCapability> = entry
                .map(|e| {
                    choices(e, ConfigOptionCategory::Model)
                        .map(|choice| AcpModelCapability {
                            id: choice.value.clone(),
                            display_name: choice.name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            let mut modes: Vec<AcpModeCapability> = entry
                .map(|e| {
                    choices(e, ConfigOptionCategory::Mode)
                        .map(|choice| AcpModeCapability {
                            id: choice.value.clone(),
                            display_name: choice.name.clone(),
                            approval_class: match classify_mode(&id, Some(&choice.value), entry) {
                                ModeDecision::Class(class) => class,
                                _ => ApprovalClass::Unattended,
                            },
                        })
                        .collect()
                })
                .unwrap_or_default();
            modes.sort_by(|a, b| a.id.cmp(&b.id));
            let mut thinking: Vec<AcpThinkingCapability> = entry
                .map(|e| {
                    choices(e, ConfigOptionCategory::ThoughtLevel)
                        .map(|choice| AcpThinkingCapability {
                            id: choice.value.clone(),
                            display_name: choice.name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            thinking.sort_by(|a, b| a.id.cmp(&b.id));
            AcpAgentCapability {
                display_name: id.clone(),
                id,
                catalog_status,
                catalog_updated_at,
                models,
                modes,
                thinking,
            }
        })
        .collect();

    serde_json::to_value(AcpCapabilitiesResponse { agents })
        .map_err(|e| DispatchError::internal(format!("serialize capabilities: {e}")))
}

async fn capabilities_probe(params: &Value) -> Result<Value, DispatchError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProbeParams {
        #[serde(default)]
        agent_id: Option<String>,
    }

    let req: ProbeParams = if params.is_null() {
        ProbeParams { agent_id: None }
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| DispatchError::invalid_params(format!("invalid probe params: {e}")))?
    };

    let targets: Vec<String> = match req.agent_id {
        Some(id) if !id.trim().is_empty() => vec![id],
        _ => {
            let catalog = load_catalog().await;
            crate::acp::AgentRegistry::with_defaults()
                .list()
                .into_iter()
                .map(|(name, _)| name.clone())
                .filter(|name| !catalog.agents.contains_key(name))
                .collect()
        }
    };

    for agent in &targets {
        if let Err(e) = crate::acp::capability_probe::probe_agent(agent).await {
            tracing::warn!(target: "acp.probe", agent = %agent, error = %e, "capability probe errored");
        }
    }

    capabilities_get().await
}

fn choices(
    entry: &AgentOptionEntry,
    category: ConfigOptionCategory,
) -> impl Iterator<Item = &crate::acp::state::ConfigOptionChoice> {
    entry
        .options
        .iter()
        .filter(move |opt| opt.category == category)
        .flat_map(|opt| opt.options.iter())
}

async fn load_catalog() -> OptionCatalog {
    tokio::task::spawn_blocking(crate::acp::option_catalog::load)
        .await
        .unwrap_or_default()
}

async fn sessions_create(
    deps: &Arc<SessionRpcDeps>,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let req: SessionsCreateRequest = serde_json::from_value(params.clone())
        .map_err(|e| DispatchError::invalid_params(format!("sessions.create params: {e}")))?;
    let plugin_id = ctx.plugin_id.clone();

    let outcome = admit_and_create(deps, ctx, &plugin_id, req).await;
    match &outcome {
        Ok(resp) => deps.policy.audit(
            &plugin_id,
            serde_json::json!({
                "op": "sessions.create",
                "decision": "ok",
                "session": resp.session_id,
                "created": resp.created,
            }),
        ),
        Err(e) => deps.policy.audit(
            &plugin_id,
            serde_json::json!({
                "op": "sessions.create",
                "decision": "denied",
                "code": e.code,
                "kind": e.data.as_ref().and_then(|d| d.get("kind")).cloned(),
            }),
        ),
    }
    let resp = outcome?;
    serde_json::to_value(resp)
        .map_err(|e| DispatchError::internal(format!("serialize create response: {e}")))
}

async fn admit_and_create(
    deps: &Arc<SessionRpcDeps>,
    ctx: &PluginRpcContext,
    plugin_id: &str,
    req: SessionsCreateRequest,
) -> Result<SessionsCreateResponse, DispatchError> {
    let catalog = load_catalog().await;
    let entry = catalog.agents.get(&req.agent_id);

    let known_agent = crate::acp::AgentRegistry::with_defaults()
        .get(&req.agent_id)
        .is_some()
        || entry.is_some();
    if !known_agent {
        return Err(DispatchError::with_kind(
            codes::INVALID_PARAMS,
            "unknown_agent",
            format!("unknown agent {:?}", req.agent_id),
        ));
    }

    let class = match classify_mode(&req.agent_id, req.mode_id.as_deref(), entry) {
        ModeDecision::Class(class) => class,
        ModeDecision::UnknownMode => {
            return Err(DispatchError::with_kind(
                codes::INVALID_PARAMS,
                "unknown_mode",
                format!(
                    "mode {:?} is neither known to the host nor advertised by {:?}",
                    req.mode_id.as_deref().unwrap_or_default(),
                    req.agent_id
                ),
            ));
        }
        ModeDecision::CatalogNotDiscovered => {
            return Err(DispatchError::with_kind(
                codes::FAILED_PRECONDITION,
                "catalog_not_discovered",
                format!(
                    "agent {:?} has not advertised its options yet; run it once or omit mode_id",
                    req.agent_id
                ),
            ));
        }
    };
    if class == ApprovalClass::Unattended && ctx.require(CAP_SESSION_UNATTENDED).is_err() {
        return Err(DispatchError {
            code: codes::POLICY_DENIED,
            message: format!(
                "mode {:?} is classified unattended and needs the session.unattended grant",
                req.mode_id.as_deref().unwrap_or_default()
            ),
            data: Some(serde_json::json!({
                "kind": "unattended_grant_required",
                "required_capability": CAP_SESSION_UNATTENDED,
                "agent_id": req.agent_id,
                "mode_id": req.mode_id,
                "approval_class": "unattended",
            })),
        });
    }

    let requested_model = req
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(model) = requested_model {
        let profile = deps.profile.clone();
        let agent_id = req.agent_id.clone();
        let pinned = tokio::task::spawn_blocking(move || {
            crate::acp::pinned_model_for_tool(
                &crate::session::config::profile_config::resolve_config_or_warn(&profile),
                &agent_id,
                None,
            )
        })
        .await
        .map_err(|e| DispatchError::internal(format!("resolve profile config: {e}")))?;
        if let Some(pinned) = pinned.filter(|pinned| pinned != model) {
            return Err(DispatchError {
                code: codes::INVALID_PARAMS,
                message: format!(
                    "model {model:?} is refused: profile {:?} pins {:?} to {pinned:?}",
                    deps.profile, req.agent_id
                ),
                data: Some(serde_json::json!({
                    "kind": "model_pinned",
                    "agent_id": req.agent_id,
                    "model_id": model,
                    "pinned_model": pinned,
                })),
            });
        }
    }

    if let (Some(model), Some(entry)) = (req.model_id.as_deref(), entry) {
        let advertised = entry.options.iter().any(|opt| {
            opt.category == ConfigOptionCategory::Model
                && opt.options.iter().any(|c| c.value == model)
        });
        if !advertised {
            return Err(DispatchError::with_kind(
                codes::INVALID_PARAMS,
                "unknown_model",
                format!("model {model:?} is not advertised by {:?}", req.agent_id),
            ));
        }
    }

    if req.initial_turn.is_some() {
        ctx.require(CAP_SESSION_PROMPT)?;
    }

    let primary = req
        .project_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let (project_path, extra_repo_paths, scratch) = match primary {
        None => {
            if req.extra_project_paths.iter().any(|p| !p.trim().is_empty()) {
                return Err(DispatchError::invalid_params(
                    "extra_project_paths requires a project_path; a scratch session takes no extra repos",
                ));
            }
            (String::new(), Vec::new(), true)
        }
        Some(primary) => {
            let extras_in: Vec<String> = req
                .extra_project_paths
                .iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if extras_in.len() > MAX_EXTRA_PROJECT_PATHS {
                return Err(DispatchError::invalid_params(format!(
                    "too many extra_project_paths ({}); max {MAX_EXTRA_PROJECT_PATHS}",
                    extras_in.len()
                )));
            }
            let primary = primary.to_string();
            tokio::task::spawn_blocking(move || {
                let canon = |p: &str| -> Result<String, DispatchError> {
                    std::fs::canonicalize(p)
                        .map_err(|e| {
                            DispatchError::invalid_params(format!("project_path {p:?}: {e}"))
                        })
                        .map(|c| c.to_string_lossy().into_owned())
                };
                let path = canon(&primary)?;
                let extras = extras_in
                    .iter()
                    .map(|p| canon(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, DispatchError>((path, extras, false))
            })
            .await
            .map_err(|e| {
                DispatchError::internal(format!("path canonicalization task failed: {e}"))
            })??
        }
    };

    let spec = StructuredSessionSpec {
        title: req.title,
        path: project_path,
        group: req.group.unwrap_or_default(),
        tool: req.agent_id.clone(),
        worktree_enabled: false,
        worktree_branch: None,
        create_new_branch: false,
        base_branch: None,
        sandbox: req.sandbox,
        sandbox_image: None,
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        extra_repo_paths,
        repo_base_branches: Vec::new(),
        scratch,
        trust_hooks: Some(false),
        custom_instruction: None,
        callback_url: None,
        idempotency_key: None,
        profile: deps.profile.clone(),
        created_by_plugin: None,
        plugin_create_idempotency: None,
        pending_initial_turn: req.initial_turn.as_ref().map(|t| t.text.clone()),
        acp_mode_id: req.mode_id.clone(),
        view: crate::session::View::Structured,
        agent_name: None,
        agent_model: req.model_id.clone(),
        agent_effort: None,
        import_acp_session_id: None,
        fork_seed: None,
        progress: None,
    };

    if let Some(key) = req.idempotency_key.as_deref() {
        match deps
            .session_service
            .probe_plugin_create_idempotency(&spec, plugin_id, key)
            .await
        {
            Ok(CreateIdempotencyProbe::Replay(instance)) => {
                return Ok(SessionsCreateResponse {
                    session_id: instance.id,
                    created: false,
                });
            }
            Ok(CreateIdempotencyProbe::New) => {}
            Err(conflict) => return Err(map_create_error(anyhow::Error::new(conflict))),
        }
    }

    // Counts sessions the plugin is still actively driving, not every session it has ever
    // created and kept around for review; a finished run left un-archived (the common case
    // for a plugin like cron, where past runs are meant to stay inspectable) must not
    // permanently occupy a concurrency slot.
    let active_sessions = {
        let instances = deps.session_service.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.created_by_plugin.as_deref() == Some(plugin_id)
                    && i.counts_toward_plugin_session_cap()
            })
            .count()
    };
    let _reservation = deps.policy.admit_create(plugin_id, active_sessions)?;

    let initial_turn_text = req.initial_turn.as_ref().map(|t| t.text.as_str());
    let (outcome, created) = deps
        .session_service
        .create_structured_session(
            spec,
            Some(plugin_id),
            req.idempotency_key.as_deref(),
            initial_turn_text,
        )
        .await
        .map_err(map_create_error)?;

    Ok(SessionsCreateResponse {
        session_id: outcome.instance.id,
        created,
    })
}

fn map_create_error(e: anyhow::Error) -> DispatchError {
    if let Some(conflict) = e.downcast_ref::<IdempotencyConflict>() {
        return DispatchError::with_kind(
            codes::CONFLICT,
            "idempotency_conflict",
            conflict.to_string(),
        );
    }
    if e.downcast_ref::<crate::server::api::sessions::HooksNeedTrust>()
        .is_some()
    {
        return DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "repo_untrusted",
            "the repository's hooks need user approval; a plugin cannot grant trust",
        );
    }
    DispatchError::internal(format!("session create failed: {e:#}"))
}

async fn sessions_turn_send(
    deps: &Arc<SessionRpcDeps>,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let req: TurnSendRequest = serde_json::from_value(params.clone())
        .map_err(|e| DispatchError::invalid_params(format!("sessions.turn.send params: {e}")))?;
    let plugin_id = ctx.plugin_id.clone();

    let result = async {
        deps.policy.admit_turn(&plugin_id)?;
        let caller = SessionCaller::Plugin {
            plugin_id: plugin_id.clone(),
        };
        let _submission = deps
            .session_service
            .admit_prompt_submission(&caller, &req.session_id)
            .await
            .map_err(|e| map_send_error(e.into()))?;

        // A target not currently counted (archived, snoozed, or not in one of the counted
        // statuses) re-occupies a slot the instant it becomes counted. Mark it pending here,
        // atomically with the cap check, under the one `instances` write lock: a plugin could
        // otherwise create sessions past the cap once idle/archived/snoozed sessions stopped
        // counting, then turn.send them all back to life at once before any of their statuses
        // caught up. Cleared by the next real status transition this session gets (see
        // `apply_status_intent` in `server::acp_events`), not by this RPC call returning:
        // `send_turn` only queues the prompt, and `Instance.status` itself updates later,
        // asynchronously, off the ACP event listener. Archived and trashed targets are exempt,
        // since a prompt never revives them.
        let marked_pending = {
            let mut instances = deps.session_service.instances.write().await;
            let needs_reservation = instances
                .iter()
                .find(|i| i.id == req.session_id)
                .is_some_and(|i| i.ensure_startable().is_ok() && !i.counts_toward_plugin_session_cap());
            if needs_reservation {
                let active_sessions = instances
                    .iter()
                    .filter(|i| {
                        i.id != req.session_id
                            && i.created_by_plugin.as_deref() == Some(plugin_id.as_str())
                            && i.counts_toward_plugin_session_cap()
                    })
                    .count();
                if active_sessions >= MAX_ACTIVE_PLUGIN_SESSIONS {
                    return Err(DispatchError::with_kind(
                        codes::RATE_LIMITED,
                        "concurrency_limited",
                        format!(
                            "plugin {plugin_id} already has {active_sessions} active or pending sessions (limit {MAX_ACTIVE_PLUGIN_SESSIONS})"
                        ),
                    ));
                }
                if let Some(target) = instances.iter_mut().find(|i| i.id == req.session_id) {
                    target.plugin_revival_pending = true;
                }
                true
            } else {
                false
            }
        };

        let woke_idle_dormant = match deps
            .session_service
            .touch_and_wake_on_prompt(&req.session_id, false)
            .await
            .idle_dormant()
        {
            Ok(woke) => woke,
            Err(blocked) => {
                if marked_pending {
                    clear_revival_pending(&deps.session_service, &req.session_id).await;
                }
                return Err(DispatchError::with_kind(
                    codes::FAILED_PRECONDITION,
                    blocked.code(),
                    blocked.to_string(),
                ));
            }
        };
        let dispatch = deps
            .session_service
            .prompt_dispatch_under_submission(&req.session_id, &req.text, woke_idle_dormant, false)
            .await;
        if let crate::acp::dispatch::PromptDispatch::Queued { reason } = dispatch {
            if !matches!(reason, crate::acp::dispatch::QueueReason::WorkerDown) {
                if marked_pending {
                    clear_revival_pending(&deps.session_service, &req.session_id).await;
                }
                return Err(DispatchError::with_kind(
                    codes::SERVICE_UNAVAILABLE,
                    "agent_busy",
                    "the session's agent is mid-turn; retry when it finishes",
                ));
            }
        }
        let sent = deps
            .session_service
            .send_turn(
                &caller,
                &req.session_id,
                SendTurnRequest { resume_authority: crate::process::worker_registry::ResumeAuthority::Explicit, text: &req.text,
                attachments: &[],
                woke_idle_dormant,
                prompt_id: None,
                synthesized: false,
                no_revive: false, },
            )
            .await;
        if let Err(SendTurnError::LoginRequired(blocked)) = &sent {
            // The kept turn revives the session after the login, so its cap reservation
            // stays: the resume's first status report clears it (`apply_status_intent`), as
            // do a user stop and a daemon restart (the mark is not persisted).
            let kept =
                keep_for_login(&deps.session_service, &req.session_id, &req.text, None).await;
            let queued_id = match kept {
                Ok(queued_id) => queued_id,
                // Nothing was kept, so there is no resume to hold the reservation for.
                Err(refusal) => {
                    if marked_pending {
                        clear_revival_pending(&deps.session_service, &req.session_id).await;
                    }
                    return Err(refusal);
                }
            };
            return Err(login_required_queued(blocked, &queued_id));
        }
        if let Err(SendTurnError::HookFailed(message)) = &sent {
            // The message is kept and delivered on the next revival, and the pending turn
            // stays. No hold is registered (a failing hook is not a login that arrives by
            // itself), so nothing would clear the cap reservation the revival attempt made:
            // clear it here.
            let kept =
                keep_for_login(&deps.session_service, &req.session_id, &req.text, None).await;
            if marked_pending {
                clear_revival_pending(&deps.session_service, &req.session_id).await;
            }
            return Err(match kept {
                Ok(queued_id) => hook_failed_queued(message, &queued_id),
                Err(refusal) => refusal,
            });
        }
        if sent.is_err() && marked_pending {
            clear_revival_pending(&deps.session_service, &req.session_id).await;
        }
        sent.map_err(map_send_error)?;
        Ok(())
    }
    .await;

    let mut audit = serde_json::json!({
        "op": "sessions.turn.send",
        "session": req.session_id,
        "decision": if result.is_ok() { "ok" } else { "denied" },
        "kind": result.as_ref().err().and_then(|e| {
            e.data.as_ref().and_then(|d| d.get("kind")).cloned()
        }),
    });
    if result
        .as_ref()
        .err()
        .and_then(|e| e.data.as_ref())
        .is_some_and(|d| d.get("queued_id").is_some())
    {
        audit["queued"] = Value::Bool(true);
    }
    deps.policy.audit(&plugin_id, audit);
    result?;
    Ok(serde_json::json!({}))
}

/// Deliver a short text message to any session, marked as sent by the plugin. Unlike
/// `sessions.turn.send` there is no ownership check: the `session.message` grant is the gate.
async fn sessions_message_send(
    deps: &Arc<SessionRpcDeps>,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let req: MessageSendRequest = serde_json::from_value(params.clone())
        .map_err(|e| DispatchError::invalid_params(format!("sessions.message.send params: {e}")))?;
    if req.message.trim().is_empty() {
        return Err(DispatchError::with_kind(
            codes::INVALID_PARAMS,
            "message_empty",
            "message must not be empty",
        ));
    }
    if req.message.len() > MAX_MESSAGE_BYTES {
        return Err(DispatchError::with_kind(
            codes::INVALID_PARAMS,
            "message_too_long",
            format!("message exceeds {} KiB", MAX_MESSAGE_BYTES / 1024),
        ));
    }
    let plugin_id = ctx.plugin_id.clone();

    let result = async {
        deps.policy.admit_message(&plugin_id)?;
        let (task_deps, task_req, task_plugin) = (Arc::clone(deps), req.clone(), plugin_id.clone());
        within_deadline(MESSAGE_DEADLINE, async move {
            deliver_message(&task_deps, &task_plugin, &task_req).await
        })
        .await
    }
    .await;

    deps.policy.audit(
        &plugin_id,
        serde_json::json!({
            "op": "sessions.message.send",
            "session": req.session_id,
            "decision": if result.is_ok() { "ok" } else { "denied" },
            "kind": result.as_ref().err().and_then(|e| {
                e.data.as_ref().and_then(|d| d.get("kind")).cloned()
            }),
        }),
    );
    serde_json::to_value(result?)
        .map_err(|e| DispatchError::internal(format!("message response encode failed: {e}")))
}

/// Bounds how long the caller waits, not the delivery: a dropped future could stop between
/// clearing the pending turn and enqueueing the message, or before the live row is synced.
async fn within_deadline<T: Send + 'static>(
    deadline: std::time::Duration,
    delivery: impl std::future::Future<Output = Result<T, DispatchError>> + Send + 'static,
) -> Result<T, DispatchError> {
    match tokio::time::timeout(deadline, tokio::spawn(delivery)).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(DispatchError::internal(format!(
            "delivery task failed: {e}"
        ))),
        Err(_) => Err(DispatchError::with_kind(
            codes::SERVICE_UNAVAILABLE,
            "delivery_timeout",
            "delivery did not finish in time; the message may still have been delivered",
        )),
    }
}

fn blocked_error(blocked: crate::session::StartBlocked) -> DispatchError {
    DispatchError::with_kind(
        codes::FAILED_PRECONDITION,
        blocked.code(),
        blocked.to_string(),
    )
}

fn session_not_found() -> DispatchError {
    map_send_error(SendTurnError::SessionNotFound)
}

async fn deliver_message(
    deps: &Arc<SessionRpcDeps>,
    plugin_id: &str,
    req: &MessageSendRequest,
) -> Result<MessageSendResponse, DispatchError> {
    let is_structured = {
        let instances = deps.session_service.instances.read().await;
        let inst = instances
            .iter()
            .find(|i| i.id == req.session_id)
            .ok_or_else(session_not_found)?;
        inst.ensure_startable().map_err(blocked_error)?;
        inst.is_structured()
    };
    if is_structured {
        return deliver_structured(&deps.session_service, plugin_id, req).await;
    }
    if deps.cityhall_mode {
        return Err(DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "terminal_unavailable",
            "terminal sessions take no plugin input in CityHall mode",
        ));
    }
    let revived = send_terminal_message(
        &deps.session_service,
        &deps.status_tx,
        &req.session_id,
        req.message.clone(),
        true,
    )
    .await
    .map_err(map_terminal_error)?;
    Ok(MessageSendResponse {
        disposition: MessageDisposition::Sent,
        revived,
    })
}

/// The user-prompt path: a busy agent steers or queues the message instead of refusing it.
async fn deliver_structured(
    service: &Arc<SessionService>,
    plugin_id: &str,
    req: &MessageSendRequest,
) -> Result<MessageSendResponse, DispatchError> {
    use crate::acp::dispatch::{PromptDispatch, QueueReason};

    let id = req.session_id.as_str();
    let caller = SessionCaller::PluginMessage {
        plugin_id: plugin_id.to_string(),
    };
    let _submission = service
        .admit_prompt_submission(&caller, id)
        .await
        .map_err(|e| map_send_error(e.into()))?;
    let woke_idle_dormant = service
        .touch_and_wake_on_prompt(id, false)
        .await
        .idle_dormant()
        .map_err(blocked_error)?;
    let was_running = service.acp_supervisor.is_running(id).await;
    let dispatch = service
        .prompt_dispatch_under_submission(id, &req.message, woke_idle_dormant, false)
        .await;
    // A stopped worker is resumed by `send_turn` below, not queued behind a drain nothing starts.
    if let PromptDispatch::Queued { reason } = dispatch {
        if reason != QueueReason::WorkerDown {
            let prompt_id = uuid::Uuid::new_v4().to_string();
            // Under the submission guard held above, so the room found stays.
            queue_room(service, id, &prompt_id, &req.message).await?;
            service
                .enqueue_prompt(
                    id,
                    prompt_id,
                    req.message.clone(),
                    Vec::new(),
                    None,
                    Some(plugin_id.to_string()),
                )
                .await
                .ok_or_else(session_not_found)?;
            // Superseded only once the message is queued.
            service.clear_pending_initial_turn(id).await;
            return Ok(MessageSendResponse {
                disposition: MessageDisposition::Queued,
                revived: woke_idle_dormant,
            });
        }
    }
    let sent = service
        .send_turn(
            &caller,
            id,
            SendTurnRequest {
                resume_authority: crate::process::worker_registry::ResumeAuthority::Explicit,
                text: &req.message,
                attachments: &[],
                woke_idle_dormant,
                prompt_id: None,
                synthesized: false,
                no_revive: false,
            },
        )
        .await;
    if let Err(SendTurnError::LoginRequired(blocked)) = &sent {
        // A pending turn (a rate-limit continuation) stays as for a `no_revive` refusal:
        // after the login it and the kept message are both delivered.
        let queued_id = keep_for_login(service, id, &req.message, Some(plugin_id)).await?;
        return Err(login_required_queued(blocked, &queued_id));
    }
    if let Err(SendTurnError::HookFailed(message)) = &sent {
        // As for the login: the message is kept and the pending turn stays.
        let queued_id = keep_for_login(service, id, &req.message, Some(plugin_id)).await?;
        return Err(hook_failed_queued(message, &queued_id));
    }
    service.clear_pending_initial_turn(id).await;
    sent.map_err(map_send_error)?;
    Ok(MessageSendResponse {
        disposition: if dispatch == PromptDispatch::Steered {
            MessageDisposition::Steered
        } else {
            MessageDisposition::Sent
        },
        revived: woke_idle_dormant || !was_running,
    })
}

/// `Ok` when the session's queue can take `text`, else the refusal every plugin enqueue path
/// answers with. Never names the message as queued: it was not kept.
async fn queue_room(
    service: &Arc<SessionService>,
    id: &str,
    prompt_id: &str,
    text: &str,
) -> Result<(), DispatchError> {
    use crate::server::session_service::{
        QueueRoom, MAX_QUEUED_PROMPTS_PER_SESSION, MAX_QUEUED_TEXT_BYTES,
    };
    match service.check_queue_room(id, prompt_id, text).await {
        QueueRoom::Available => Ok(()),
        QueueRoom::TextTooLarge => Err(DispatchError::with_kind(
            codes::INVALID_PARAMS,
            "message_too_long",
            format!(
                "the message exceeds the {} KiB limit and was not kept",
                MAX_QUEUED_TEXT_BYTES / 1024
            ),
        )),
        QueueRoom::Full => Err(DispatchError::with_kind(
            codes::RATE_LIMITED,
            "queue_full",
            format!(
                "the session already holds {MAX_QUEUED_PROMPTS_PER_SESSION} waiting prompts; the message was not kept, retry when it drains"
            ),
        )),
    }
}

/// Queues a turn refused for the sandbox login and returns its queue id; the drain
/// delivers it once the held session is resumed after the login.
async fn keep_for_login(
    service: &Arc<SessionService>,
    id: &str,
    text: &str,
    origin_plugin: Option<&str>,
) -> Result<String, DispatchError> {
    let prompt_id = uuid::Uuid::new_v4().to_string();
    // Under the submission guard the caller holds, so the room found stays. A refusal is
    // not the login-required answer: that one says the message is queued.
    queue_room(service, id, &prompt_id, text).await?;
    service
        .enqueue_prompt(
            id,
            prompt_id,
            text.to_string(),
            Vec::new(),
            None,
            origin_plugin.map(str::to_string),
        )
        .await
        .map(|entry| entry.id)
        .ok_or_else(session_not_found)
}

/// The refusal for a turn [`keep_for_login`] kept: retrying would queue it twice.
fn login_required_queued(
    blocked: &crate::session::SandboxLoginRequired,
    queued_id: &str,
) -> DispatchError {
    let mut error = map_send_error(SendTurnError::LoginRequired(blocked.clone()));
    if let Some(data) = error.data.as_mut() {
        data["queued_id"] = Value::String(queued_id.to_string());
    }
    error
}

/// The refusal for a launch whose `host_hooks.before_start` hook failed: the hook's own
/// error, with the id the kept message is queued under.
fn hook_failed_queued(message: &str, queued_id: &str) -> DispatchError {
    let mut error = DispatchError::with_kind(
        codes::FAILED_PRECONDITION,
        "before_start_hook_failed",
        message.to_string(),
    );
    if let Some(data) = error.data.as_mut() {
        data["queued_id"] = Value::String(queued_id.to_string());
    }
    error
}

/// The refusal for a launch that needs the sandbox login, naming the tool it is for.
fn login_required_error(
    blocked: &crate::session::SandboxLoginRequired,
    consequence: &str,
) -> DispatchError {
    DispatchError::with_kind(
        codes::FAILED_PRECONDITION,
        crate::session::SandboxLoginRequired::CODE,
        format!("{blocked}; {consequence}"),
    )
}

fn map_terminal_error(e: SendKeysError) -> DispatchError {
    match e {
        SendKeysError::NotFound | SendKeysError::Gone => session_not_found(),
        SendKeysError::Blocked(blocked) => blocked_error(blocked),
        SendKeysError::LoginRequired(blocked) => login_required_error(
            &blocked,
            "terminal sessions have no queue, retry after the login",
        ),
        SendKeysError::NotRunning => DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "session_not_running",
            "the session's terminal is not running",
        ),
        SendKeysError::ResumeFailed(sid) => DispatchError {
            code: codes::FAILED_PRECONDITION,
            message: format!("resume failed for sid {sid}; preserved for explicit retry"),
            data: Some(serde_json::json!({
                "kind": "resume_failed",
                "resume_session_id": sid,
            })),
        },
        SendKeysError::Transient(status) => DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "session_transient",
            format!("session is {status:?}; retry when it settles"),
        ),
        SendKeysError::StructuredView => {
            DispatchError::internal("terminal send reached a structured session")
        }
        SendKeysError::Tmux(e) => DispatchError::internal(format!("terminal send failed: {e:#}")),
        SendKeysError::Internal => DispatchError::internal("terminal send failed"),
    }
}

fn map_send_error(e: SendTurnError) -> DispatchError {
    match e {
        SendTurnError::SessionNotFound => DispatchError::with_kind(
            codes::INVALID_PARAMS,
            "session_not_found",
            "session not found",
        ),
        SendTurnError::NotOwner => DispatchError::with_kind(
            codes::FORBIDDEN,
            "not_owner",
            "the session was not created by the calling plugin",
        ),
        SendTurnError::ModeApplication(e) => DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "mode_application_failed",
            format!("mode application failed: {e}"),
        ),
        SendTurnError::ResumeFailed(e) => DispatchError::with_kind(
            codes::SERVICE_UNAVAILABLE,
            "worker_not_ready",
            format!("worker resume failed: {e}"),
        ),
        SendTurnError::WorkerNotReady => DispatchError::with_kind(
            codes::SERVICE_UNAVAILABLE,
            "worker_not_ready",
            "worker not ready; retry",
        ),
        // Unreachable: this plugin surface never sets `no_revive`.
        SendTurnError::RevivalRefused => DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "no_revive",
            "reviving a stopped worker is required",
        ),
        // Both callers keep the message and add its id (`login_required_queued`).
        SendTurnError::LoginRequired(blocked) => login_required_error(
            &blocked,
            "the message is queued and is delivered after the login",
        ),
        SendTurnError::HookFailed(message) => DispatchError::with_kind(
            codes::FAILED_PRECONDITION,
            "before_start_hook_failed",
            message,
        ),
        SendTurnError::Send(e) => DispatchError::internal(format!("prompt forward failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::automation_policy::AutomationPolicy;
    use crate::session::{Instance, Status};

    fn ctx_with(caps: &[&str]) -> PluginRpcContext {
        PluginRpcContext {
            plugin_id: "cron".to_string(),
            granted_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ui_contributions: std::collections::HashSet::new(),
            ui_generation: 1,
        }
    }

    fn test_deps(prior: Vec<Instance>) -> (Arc<SessionRpcDeps>, tempfile::TempDir) {
        let (deps, _state, dir) = test_deps_with_state(prior);
        (deps, dir)
    }

    fn test_deps_with_state(
        prior: Vec<Instance>,
    ) -> (
        Arc<SessionRpcDeps>,
        Arc<crate::server::AppState>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = crate::server::test_support::build_test_app_state(prior);
        let policy =
            Arc::new(AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("policy"));
        (
            Arc::new(SessionRpcDeps {
                session_service: state.session_service.clone(),
                policy,
                profile: "test".to_string(),
                cityhall_mode: false,
                status_tx: state.status_tx.clone(),
            }),
            state,
            dir,
        )
    }

    fn write_app_config(body: &str) {
        let path = crate::session::get_app_dir()
            .expect("isolated app dir")
            .join("config.toml");
        std::fs::write(&path, body).expect("write config");
    }

    fn kind(e: &DispatchError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn authz_matrix_capability_gates() {
        let (deps, _dir) = test_deps(Vec::new());
        let none = ctx_with(&[]);
        for method in [
            "acp.capabilities.get",
            "acp.capabilities.probe",
            "sessions.create",
            "sessions.turn.send",
            "sessions.message.send",
        ] {
            let err = dispatch(&deps, &none, method, &serde_json::json!({}))
                .await
                .expect_err("must be refused without the capability");
            assert_eq!(err.code, codes::FORBIDDEN, "{method}");
            assert_eq!(kind(&err), "capability_missing", "{method}");
        }
        let wrong = ctx_with(&["session.prompt"]);
        let err = dispatch(&deps, &wrong, "sessions.create", &serde_json::json!({}))
            .await
            .expect_err("session.prompt must not grant sessions.create");
        assert_eq!(err.code, codes::FORBIDDEN);

        let unattended = serde_json::json!({
            "agent_id": "claude",
            "project_path": "/tmp",
            "mode_id": "bypassPermissions",
        });
        let err = dispatch(
            &deps,
            &ctx_with(&["session.create"]),
            "sessions.create",
            &unattended,
        )
        .await
        .expect_err("unattended without the grant must be refused");
        assert_eq!(err.code, codes::POLICY_DENIED);
        assert_eq!(kind(&err), "unattended_grant_required");
    }

    #[tokio::test]
    async fn invalid_params_are_rejected() {
        let (deps, _dir) = test_deps(Vec::new());
        let cases = [
            (
                "unknown create field",
                "session.create",
                "sessions.create",
                serde_json::json!({
                    "agent_id": "claude",
                    "project_path": "/tmp",
                    "allow_untrusted": true,
                }),
            ),
            (
                "scratch session with extra repos",
                "session.create",
                "sessions.create",
                serde_json::json!({ "agent_id": "claude", "extra_project_paths": ["/tmp"] }),
            ),
            (
                "unknown probe param",
                "acp.capabilities.probe",
                "acp.capabilities.probe",
                serde_json::json!({ "bogus": 1 }),
            ),
        ];
        for (label, capability, method, params) in cases {
            let err = dispatch(&deps, &ctx_with(&[capability]), method, &params)
                .await
                .expect_err(label);
            assert_eq!(err.code, codes::INVALID_PARAMS, "{label}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn create_refuses_a_model_off_the_profile_pin_including_through_a_wrapper() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        write_app_config(
            "[session.agent_detect_as]\nmy-claude = \"claude\"\n\n\
             [acp.acp_defaults.claude]\nmodel = \"claude-pinned\"\npin_model = true\n",
        );
        crate::acp::option_catalog::record("my-claude", &[], "2026-01-01T00:00:00Z".into())
            .expect("seed catalog");
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.create", "session.unattended"]);

        let err = dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({
                "agent_id": "claude",
                "project_path": "/tmp",
                "model_id": "claude-other",
            }),
        )
        .await
        .expect_err("a model off the pin must be refused");
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert_eq!(kind(&err), "model_pinned");
        let data = err.data.expect("typed error data");
        assert_eq!(data["agent_id"], "claude");
        assert_eq!(data["model_id"], "claude-other");
        assert_eq!(data["pinned_model"], "claude-pinned");

        for params in [
            serde_json::json!({
                "agent_id": "claude",
                "project_path": "/nonexistent/aoe-pin-gate",
                "model_id": "claude-pinned",
            }),
            serde_json::json!({
                "agent_id": "claude",
                "project_path": "/nonexistent/aoe-pin-gate",
            }),
        ] {
            let err = dispatch(&deps, &ctx, "sessions.create", &params)
                .await
                .expect_err("a missing project path is refused");
            assert_eq!(err.code, codes::INVALID_PARAMS, "{params}");
            assert_ne!(kind(&err), "model_pinned", "{params}");
            assert!(err.message.contains("project_path"), "{}", err.message);
        }

        // A wrapper agent is held to the pin of the agent it spawns.
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({
                "agent_id": "my-claude",
                "project_path": "/tmp",
                "model_id": "claude-other",
            }),
        )
        .await
        .expect_err("a model off the base agent's pin must be refused");
        assert_eq!(kind(&err), "model_pinned", "{}", err.message);
        let data = err.data.expect("typed error data");
        assert_eq!(data["agent_id"], "my-claude");
        assert_eq!(data["pinned_model"], "claude-pinned");
    }

    #[tokio::test]
    async fn create_at_concurrency_limit_denies_a_new_key() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS)
            .map(|n| {
                let mut i = Instance::new("scheduled", "/tmp/aoe-2897-project");
                i.id = format!("sess-{n}");
                i.created_by_plugin = Some("cron".to_string());
                i.status = Status::Running;
                i
            })
            .collect();
        let (deps, _dir) = test_deps(prior);
        let ctx = ctx_with(&["session.create"]);
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({ "agent_id": "claude", "project_path": "/tmp" }),
        )
        .await
        .expect_err("must be denied at the active-session limit");
        assert_eq!(err.code, codes::RATE_LIMITED);
        assert_eq!(kind(&err), "concurrency_limited");
    }

    #[tokio::test]
    async fn create_ignores_finished_runs_left_unarchived() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        // Unlike the denied case above, this path actually persists a session to disk, so it
        // needs its own app dir: HOME is process-global, and running the full suite races this
        // against every other test that installs or restores it.
        let _home = crate::session::test_support::isolate_app_dir();
        // Past runs default to `Status::Idle` and are kept around, unarchived, for the user
        // to review; they must not count against the concurrency limit like `sess-1` does.
        let mut prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS)
            .map(|n| {
                let mut i = Instance::new("past run", "/tmp/aoe-2897-project");
                i.id = format!("sess-idle-{n}");
                i.created_by_plugin = Some("cron".to_string());
                i
            })
            .collect();
        let mut running = Instance::new("scheduled", "/tmp/aoe-2897-project");
        running.id = "sess-1".to_string();
        running.created_by_plugin = Some("cron".to_string());
        running.status = Status::Running;
        prior.push(running);
        let (deps, _dir) = test_deps(prior);
        let ctx = ctx_with(&["session.create"]);
        dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({ "agent_id": "claude", "project_path": "/tmp" }),
        )
        .await
        .expect("idle, unarchived history must not occupy a concurrency slot");
    }

    fn message_params(session: &str, message: &str) -> Value {
        serde_json::json!({ "session_id": session, "message": message })
    }

    /// A live structured session no plugin created, with a recording fake worker.
    async fn foreign_live_session(
        id: &str,
    ) -> (
        Arc<SessionRpcDeps>,
        Arc<crate::server::AppState>,
        tempfile::TempDir,
        Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) {
        let mut foreign = Instance::new("foreign", "/tmp/aoe-msg-project");
        foreign.id = id.to_string();
        foreign.view = crate::session::View::Structured;
        foreign.agent_name = Some("claude".to_string());
        let (deps, state, dir) = test_deps_with_state(vec![foreign]);
        let cmds = deps
            .session_service
            .acp_supervisor
            .test_insert_worker_cmd_recording(id)
            .await;
        (deps, state, dir, cmds)
    }

    #[tokio::test]
    async fn message_send_needs_its_own_capability() {
        let (deps, _dir) = test_deps(Vec::new());
        let err = dispatch(
            &deps,
            &ctx_with(&["session.prompt", "session.create"]),
            "sessions.message.send",
            &message_params("s", "hi"),
        )
        .await
        .expect_err("session.prompt must not grant sessions.message.send");
        assert_eq!(err.code, codes::FORBIDDEN);
        assert_eq!(kind(&err), "capability_missing");
        let err = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.turn.send",
            &serde_json::json!({ "session_id": "s", "text": "hi" }),
        )
        .await
        .expect_err("session.message must not grant sessions.turn.send");
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    #[tokio::test]
    async fn message_send_validates_the_message_before_admitting_it() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.message"]);
        let oversize = "x".repeat(MAX_MESSAGE_BYTES + 1);
        let at_limit = "x".repeat(MAX_MESSAGE_BYTES);
        for (label, message, want) in [
            ("empty", "", Some("message_empty")),
            ("whitespace", " \n\t ", Some("message_empty")),
            ("oversize", oversize.as_str(), Some("message_too_long")),
            // Passes validation and only then reports the missing session.
            ("at the limit", at_limit.as_str(), None),
        ] {
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.message.send",
                &message_params("s", message),
            )
            .await
            .expect_err(label);
            match want {
                Some(want) => {
                    assert_eq!(err.code, codes::INVALID_PARAMS, "{label}");
                    assert_eq!(kind(&err), want, "{label}");
                }
                None => assert_eq!(kind(&err), "session_not_found", "{label}"),
            }
        }
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.message.send",
            &serde_json::json!({ "session_id": "s", "message": "hi", "text": "x" }),
        )
        .await
        .expect_err("unknown fields are rejected");
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn message_send_rate_limits_per_plugin_in_its_own_bucket() {
        use crate::plugin::automation_policy::{
            MAX_PLUGIN_MESSAGES_PER_HOUR, MAX_PLUGIN_TURNS_PER_HOUR,
        };
        let (deps, _dir) = test_deps(Vec::new());
        for _ in 0..MAX_PLUGIN_MESSAGES_PER_HOUR {
            deps.policy.admit_message("cron").expect("under the rate");
        }
        let err = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("s", "hi"),
        )
        .await
        .expect_err("bucket exhausted");
        assert_eq!(err.code, codes::RATE_LIMITED);
        assert_eq!(kind(&err), "rate_limited");
        const { assert!(MAX_PLUGIN_MESSAGES_PER_HOUR > MAX_PLUGIN_TURNS_PER_HOUR) };
        deps.policy
            .admit_turn("cron")
            .expect("messages do not drain the turn bucket");
    }

    #[tokio::test]
    async fn message_send_reports_missing_and_shelved_sessions_with_stable_kinds() {
        let _home = crate::session::test_support::isolate_app_dir();
        type Shelve = (&'static str, fn(&mut Instance));
        let shelves: [Shelve; 2] = [
            ("session_archived", Instance::archive),
            ("session_trashed", Instance::trash),
        ];
        let mut prior = Vec::new();
        for (want, shelve) in shelves {
            for structured in [false, true] {
                let mut inst = Instance::new("shelved", "/tmp/aoe-msg-project");
                inst.id = format!("{want}-{structured}");
                if structured {
                    inst.view = crate::session::View::Structured;
                    inst.agent_name = Some("aoe-no-such-agent-msg".to_string());
                }
                shelve(&mut inst);
                prior.push(inst);
            }
        }
        let (deps, state, _dir) = test_deps_with_state(prior.clone());
        let ctx = ctx_with(&["session.message"]);
        for inst in &prior {
            let want = if inst.is_archived() {
                "session_archived"
            } else {
                "session_trashed"
            };
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.message.send",
                &message_params(&inst.id, "ping"),
            )
            .await
            .expect_err("a shelved session is never woken");
            assert_eq!(kind(&err), want, "{}", inst.id);
            assert_eq!(err.code, codes::FAILED_PRECONDITION, "{}", inst.id);
            let instances = state.instances.read().await;
            let after = instances.iter().find(|i| i.id == inst.id).unwrap();
            assert_eq!(after.archived_at, inst.archived_at, "{}", inst.id);
            assert_eq!(after.trashed_at, inst.trashed_at, "{}", inst.id);
            assert_eq!(after.last_accessed_at, inst.last_accessed_at, "{}", inst.id);
        }
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.message.send",
            &message_params("sess-gone", "ping"),
        )
        .await
        .expect_err("a missing session is an error");
        assert_eq!(kind(&err), "session_not_found");
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn message_send_delivers_to_a_session_the_plugin_did_not_create() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (deps, _state, _dir, cmds) = foreign_live_session("sess-msg-idle").await;
        let out = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("sess-msg-idle", "PR #7 was merged"),
        )
        .await
        .expect("any session is a valid target");
        assert_eq!(
            out,
            serde_json::json!({ "disposition": "sent", "revived": false })
        );
        // The fake worker records on its own task, so wait for the observable result.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while *cmds.lock().unwrap() != ["prompt"] {
            assert!(
                std::time::Instant::now() < deadline,
                "the prompt must reach the worker, saw {:?}",
                cmds.lock().unwrap()
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn message_send_queues_behind_a_busy_agent_instead_of_refusing() {
        use crate::acp::state::Event;
        use crate::acp::supervisor::BroadcastSink;
        let _home = crate::session::test_support::isolate_app_dir();
        let (deps, state, _dir, _cmds) = foreign_live_session("sess-msg-busy").await;
        let sink = crate::acp::supervisor::ChannelSink {
            tx: state.acp_events_tx.clone(),
            event_store: Arc::clone(&state.acp_event_store),
            control_cache: Arc::clone(&state.acp_control_cache),
        };
        assert!(sink.publish_persisted(
            "sess-msg-busy",
            1,
            &Event::UserPromptSent {
                text: "the user's turn".into(),
                attachments: Vec::new(),
                prompt_id: None,
                synthesized: false,
                origin_plugin: None,
            },
        ));
        let out = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("sess-msg-busy", "PR #7 was merged"),
        )
        .await
        .expect("a busy agent queues the message");
        assert_eq!(out["disposition"], "queued");
        let queue = deps
            .session_service
            .queued_prompts_snapshot("sess-msg-busy")
            .await;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].text, "PR #7 was merged");
        assert_eq!(queue[0].origin_plugin.as_deref(), Some("cron"));
    }

    #[tokio::test]
    async fn message_send_marks_the_published_prompt_as_plugin_originated() {
        use crate::acp::state::Event;
        let _home = crate::session::test_support::isolate_app_dir();
        let (deps, state, _dir, _cmds) = foreign_live_session("sess-msg-origin").await;
        dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("sess-msg-origin", "PR #7 was merged"),
        )
        .await
        .expect("delivered");
        let origin = state
            .acp_event_store
            .replay_from("sess-msg-origin", 0)
            .into_iter()
            .find_map(|(_, e)| match e {
                Event::UserPromptSent { origin_plugin, .. } => Some(origin_plugin),
                _ => None,
            })
            .expect("the prompt is persisted before it is forwarded");
        assert_eq!(origin.as_deref(), Some("cron"));
    }

    #[tokio::test]
    async fn message_send_wakes_a_parked_structured_session() {
        let _home = crate::session::test_support::isolate_app_dir();
        type Park = (&'static str, fn(&mut Instance));
        let parks: Vec<Park> = vec![
            ("idle-dormant", |i| i.mark_idle_dormant()),
            ("snoozed", |i| {
                i.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1))
            }),
            ("stopped, nothing to clear", |_| {}),
        ];
        for (label, park) in parks {
            let mut parked = Instance::new("parked", "/tmp/aoe-msg-project");
            parked.id = "sess-msg-parked".to_string();
            parked.view = crate::session::View::Structured;
            parked.agent_name = Some("aoe-no-such-agent-msg".to_string());
            park(&mut parked);
            let (deps, state, _dir) = test_deps_with_state(vec![parked]);
            let result = dispatch(
                &deps,
                &ctx_with(&["session.message"]),
                "sessions.message.send",
                &message_params("sess-msg-parked", "wake up"),
            )
            .await;
            // The fake agent cannot start, so the resume itself may fail; what matters is that
            // the wake was attempted and not mistaken for a missing or busy session.
            if let Err(err) = &result {
                assert!(
                    ["worker_not_ready", "internal"].contains(&kind(err).as_str())
                        || err.code == codes::INTERNAL_ERROR,
                    "{label}: {err:?}"
                );
            }
            let instances = state.instances.read().await;
            let inst = instances
                .iter()
                .find(|i| i.id == "sess-msg-parked")
                .unwrap();
            assert!(
                !inst.is_idle_dormant() && !inst.is_snoozed(),
                "{label}: the message clears the park"
            );
            assert!(inst.last_accessed_at.is_some(), "{label}");
        }
    }

    #[tokio::test]
    async fn message_send_refuses_terminal_sessions_in_cityhall_mode() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut terminal = Instance::new("terminal", "/tmp/aoe-msg-project");
        terminal.id = "sess-msg-terminal".to_string();
        let (deps, _dir) = test_deps(vec![terminal]);
        let cityhall = Arc::new(SessionRpcDeps {
            session_service: deps.session_service.clone(),
            policy: deps.policy.clone(),
            profile: deps.profile.clone(),
            cityhall_mode: true,
            status_tx: deps.status_tx.clone(),
        });
        let err = dispatch(
            &cityhall,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("sess-msg-terminal", "ping"),
        )
        .await
        .expect_err("terminal input is closed in CityHall mode");
        assert_eq!(kind(&err), "terminal_unavailable");
        assert_eq!(err.code, codes::FAILED_PRECONDITION);
    }

    #[tokio::test]
    async fn a_missed_deadline_does_not_cancel_the_delivery() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (done, finished) = tokio::sync::oneshot::channel::<()>();
        let err = within_deadline(std::time::Duration::from_millis(10), async move {
            released.await.expect("released");
            done.send(()).expect("test still waiting");
            Ok(())
        })
        .await
        .expect_err("the delivery is still blocked past the deadline");
        assert_eq!(kind(&err), "delivery_timeout");
        assert_eq!(err.code, codes::SERVICE_UNAVAILABLE);

        release.send(()).expect("delivery task still alive");
        tokio::time::timeout(std::time::Duration::from_secs(10), finished)
            .await
            .expect("the delivery must run to completion after the caller gave up")
            .expect("delivery completed");
    }

    #[test]
    fn terminal_send_errors_map_to_documented_kinds() {
        let cases: Vec<(SendKeysError, &str)> = vec![
            (SendKeysError::NotFound, "session_not_found"),
            (SendKeysError::Gone, "session_not_found"),
            (
                SendKeysError::Blocked(crate::session::StartBlocked::Archived),
                "session_archived",
            ),
            (
                SendKeysError::Blocked(crate::session::StartBlocked::Trashed),
                "session_trashed",
            ),
            (SendKeysError::NotRunning, "session_not_running"),
            (
                SendKeysError::Transient(Status::Starting),
                "session_transient",
            ),
            (SendKeysError::ResumeFailed("sid-1".into()), "resume_failed"),
            (
                SendKeysError::LoginRequired(crate::session::test_support::login_required(
                    "claude",
                )),
                "sandbox_login_required",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(kind(&map_terminal_error(err)), want);
        }
        let resume = map_terminal_error(SendKeysError::ResumeFailed("sid-1".into()));
        assert_eq!(resume.data.unwrap()["resume_session_id"], "sid-1");
        let login = map_terminal_error(SendKeysError::LoginRequired(
            crate::session::test_support::login_required("claude"),
        ));
        assert_eq!(login.code, codes::FAILED_PRECONDITION);
        assert_eq!(
            login.message,
            format!(
                "{}; terminal sessions have no queue, retry after the login",
                crate::session::test_support::login_required("claude")
            )
        );
    }

    #[tokio::test]
    async fn turn_send_maps_ownership_and_missing_session_without_leaking_locks() {
        let mut user_session = Instance::new("user-owned", "/tmp/aoe-2897-project");
        user_session.id = "sess-user".to_string();
        let mut other_session = Instance::new("other-owned", "/tmp/aoe-2897-project");
        other_session.id = "sess-other".to_string();
        other_session.created_by_plugin = Some("other-plugin".to_string());
        let (deps, _dir) = test_deps(vec![user_session, other_session]);
        let ctx = ctx_with(&["session.prompt"]);

        for (session, expected_kind, expected_code) in [
            ("sess-user", "not_owner", codes::FORBIDDEN),
            ("sess-other", "not_owner", codes::FORBIDDEN),
            ("sess-gone", "session_not_found", codes::INVALID_PARAMS),
        ] {
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": session, "text": "hi" }),
            )
            .await
            .expect_err("must be refused");
            assert_eq!(err.code, expected_code, "{session}");
            assert_eq!(kind(&err), expected_kind, "{session}");
        }
        assert_eq!(
            deps.session_service.prompt_locks_len().await,
            0,
            "an id that was never admitted must not leave a lock-registry entry behind"
        );
    }

    #[tokio::test]
    async fn turn_send_refuses_a_turn_another_submission_already_started() {
        use std::time::Duration;

        let _home = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("plugin-3649", "/tmp/aoe-3649-plugin");
        inst.id = "sess-3649".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        inst.created_by_plugin = Some("cron".to_string());
        let (deps, _dir) = test_deps(vec![inst]);
        let cmds = deps
            .session_service
            .acp_supervisor
            .test_insert_worker_cmd_recording("sess-3649")
            .await;

        let winner = deps.session_service.prompt_submission("sess-3649").await;
        let mut claims = deps.session_service.watch_submission_claims();
        let context = ctx_with(&["session.prompt"]);
        let params = serde_json::json!({ "session_id": "sess-3649", "text": "hi" });
        let send = dispatch(&deps, &context, "sessions.turn.send", &params);
        tokio::pin!(send);
        assert!(
            futures_util::poll!(&mut send).is_pending(),
            "the contender must reach the held submission lock before deciding"
        );
        assert_eq!(claims.try_recv().unwrap(), "sess-3649");

        deps.session_service
            .acp_supervisor
            .publish_user_prompt_with_attachments(
                "sess-3649",
                "the winning turn".into(),
                &[],
                None,
                false,
                None,
            )
            .await;
        drop(winner);

        let err = tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .expect("the RPC must finish once the winner releases the session")
            .expect_err("a turn that cannot start must not report success");
        assert_eq!(err.code, codes::SERVICE_UNAVAILABLE);
        assert_eq!(kind(&err), "agent_busy");
        assert_eq!(
            *cmds.lock().expect("cmd log mutex poisoned"),
            Vec::<&'static str>::new(),
            "nothing may reach the agent behind the running turn"
        );
    }

    #[tokio::test]
    async fn turn_send_refuses_a_foreign_session_in_every_control_state() {
        use crate::acp::state::Event;
        use crate::acp::supervisor::BroadcastSink;

        let mut foreign = Instance::new("other-owned", "/tmp/aoe-3685-plugin");
        foreign.id = "sess-3685".to_string();
        foreign.view = crate::session::View::Structured;
        foreign.agent_name = Some("claude".to_string());
        foreign.created_by_plugin = Some("other-plugin".to_string());
        let (deps, state, _dir) = test_deps_with_state(vec![foreign]);
        deps.session_service
            .acp_supervisor
            .test_insert_worker("sess-3685")
            .await;
        let sink = crate::acp::supervisor::ChannelSink {
            tx: state.acp_events_tx.clone(),
            event_store: Arc::clone(&state.acp_event_store),
            control_cache: Arc::clone(&state.acp_control_cache),
        };
        let ctx = ctx_with(&["session.prompt"]);

        let mut seq = 0;
        let mut record = |event: Event| {
            seq += 1;
            assert!(
                sink.publish_persisted("sess-3685", seq, &event),
                "publish must reach the event store"
            );
        };
        let prompt = || Event::UserPromptSent {
            text: "the owner's turn".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
        };
        for (label, events, expected) in [
            ("idle", vec![], crate::acp::dispatch::PromptDispatch::Sent),
            (
                "busy",
                vec![prompt()],
                crate::acp::dispatch::PromptDispatch::Queued {
                    reason: crate::acp::dispatch::QueueReason::TurnActive,
                },
            ),
            (
                "cancelling",
                vec![Event::CancelRequested {
                    escalates_at: chrono::Utc::now(),
                }],
                crate::acp::dispatch::PromptDispatch::Queued {
                    reason: crate::acp::dispatch::QueueReason::Cancelling,
                },
            ),
            (
                "compacting",
                vec![
                    Event::Stopped {
                        reason: "cancelled".into(),
                    },
                    prompt(),
                    Event::ConversationCompactionStarted,
                ],
                crate::acp::dispatch::PromptDispatch::Queued {
                    reason: crate::acp::dispatch::QueueReason::Compacting,
                },
            ),
        ] {
            for event in events {
                record(event);
            }
            assert_eq!(
                crate::acp::dispatch::decide(
                    &deps.session_service.fold_control_state("sess-3685").await,
                    crate::acp::dispatch::WorkerLiveness {
                        running: true,
                        idle_dormant: false,
                        rate_limit_parked: false,
                    },
                ),
                expected,
                "{label}: the session is not in the state this row exercises"
            );
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": "sess-3685", "text": "hi" }),
            )
            .await
            .expect_err("a foreign session must be refused");
            assert_eq!(kind(&err), "not_owner", "{label}");
            assert_eq!(err.code, codes::FORBIDDEN, "{label}");
        }
    }

    #[tokio::test]
    async fn turn_send_wakes_a_parked_session() {
        let _home = crate::session::test_support::isolate_app_dir();
        type Park = (&'static str, fn(&mut Instance));
        let parks: Vec<Park> = vec![
            ("idle-dormant", |i| i.mark_idle_dormant()),
            ("snoozed", |i| {
                i.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1))
            }),
            ("stopped by the user, nothing to clear", |_| {}),
        ];
        for (label, park) in parks {
            let mut parked = Instance::new("parked-owned", "/tmp/aoe-3686-plugin");
            parked.id = "sess-3686".to_string();
            parked.view = crate::session::View::Structured;
            parked.agent_name = Some("aoe-no-such-agent-3686".to_string());
            parked.created_by_plugin = Some("cron".to_string());
            park(&mut parked);
            let (deps, state, _dir) = test_deps_with_state(vec![parked]);
            let ctx = ctx_with(&["session.prompt"]);

            let result = dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": "sess-3686", "text": "wake up" }),
            )
            .await;
            if let Err(err) = &result {
                assert_ne!(
                    kind(err),
                    "session_not_found",
                    "{label}: a parked session is resumed, not reported missing: {err:?}"
                );
            }
            let instances = state.instances.read().await;
            let inst = instances.iter().find(|i| i.id == "sess-3686").unwrap();
            assert!(
                !inst.is_idle_dormant() && !inst.is_archived() && !inst.is_snoozed(),
                "{label}: the turn clears the park"
            );
            assert!(inst.last_accessed_at.is_some(), "{label}");
        }
    }

    #[tokio::test]
    async fn turn_send_denies_reviving_a_parked_session_at_the_concurrency_cap() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let _home = crate::session::test_support::isolate_app_dir();

        // A successful wake clears snoozed/idle-dormant, so neither park kind can be treated as
        // exempt: by the time a later resumability check would see the flag, it is gone. Idle
        // and Stopped have no such flag to clear, but are just as capable of a dead worker.
        type Park = (&'static str, fn(&mut Instance));
        let parks: Vec<Park> = vec![
            ("snoozed", |i| {
                i.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1))
            }),
            ("idle-dormant", |i| i.mark_idle_dormant()),
            ("idle", |i| i.status = Status::Idle),
            ("stopped", |i| i.status = Status::Stopped),
        ];
        for (label, park) in parks {
            let mut prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS)
                .map(|n| {
                    let mut i = Instance::new("scheduled", "/tmp/aoe-4120-plugin");
                    i.id = format!("sess-running-{n}");
                    i.created_by_plugin = Some("cron".to_string());
                    i.status = Status::Running;
                    i
                })
                .collect();

            let mut resting = Instance::new("parked-owned", "/tmp/aoe-4120-plugin");
            resting.id = "sess-resting".to_string();
            resting.view = crate::session::View::Structured;
            resting.agent_name = Some("aoe-no-such-agent-4120".to_string());
            resting.created_by_plugin = Some("cron".to_string());
            park(&mut resting);
            prior.push(resting);

            let (deps, state, _dir) = test_deps_with_state(prior);
            let ctx = ctx_with(&["session.prompt"]);

            let err = match dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": "sess-resting", "text": "wake up" }),
            )
            .await
            {
                Err(e) => e,
                Ok(_) => panic!("{label}: reviving into a full plugin quota must be denied"),
            };
            assert_eq!(err.code, codes::RATE_LIMITED, "{label}");
            assert_eq!(kind(&err), "concurrency_limited", "{label}");

            let instances = state.instances.read().await;
            let inst = instances.iter().find(|i| i.id == "sess-resting").unwrap();
            let still_parked = match label {
                "snoozed" => inst.is_snoozed(),
                "idle-dormant" => inst.is_idle_dormant(),
                "idle" => inst.status == Status::Idle,
                "stopped" => inst.status == Status::Stopped,
                _ => unreachable!("unlisted park kind {label}"),
            };
            assert!(
                still_parked,
                "{label}: a denied revival must leave the park in place"
            );
        }
    }

    /// #4116: a prompt never wakes an archived or trashed target. It neither consumes nor is
    /// denied by the cap, leaves no pending mark, and leaves the row untouched.
    #[tokio::test]
    async fn turn_send_refuses_a_shelved_target_without_touching_the_cap() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let _home = crate::session::test_support::isolate_app_dir();

        let shelves: [(fn(&mut Instance), &str); 2] = [
            (Instance::archive, "session_archived"),
            (Instance::trash, "session_trashed"),
        ];
        for (shelve, want) in shelves {
            let mut prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS)
                .map(|n| {
                    let mut i = Instance::new("scheduled", "/tmp/aoe-4120-plugin");
                    i.id = format!("sess-running-{n}");
                    i.created_by_plugin = Some("cron".to_string());
                    i.status = Status::Running;
                    i
                })
                .collect();

            let mut shelved = Instance::new("parked-owned", "/tmp/aoe-4120-plugin");
            shelved.id = "sess-shelved".to_string();
            shelved.view = crate::session::View::Structured;
            shelved.agent_name = Some("aoe-no-such-agent-4120".to_string());
            shelved.created_by_plugin = Some("cron".to_string());
            shelve(&mut shelved);
            prior.push(shelved.clone());

            let (deps, state, _dir) = test_deps_with_state(prior);
            let err = dispatch(
                &deps,
                &ctx_with(&["session.prompt"]),
                "sessions.turn.send",
                &serde_json::json!({ "session_id": "sess-shelved", "text": "wake up" }),
            )
            .await
            .expect_err("a shelved session must not be woken");
            assert_eq!(kind(&err), want);
            let instances = state.instances.read().await;
            let inst = instances.iter().find(|i| i.id == "sess-shelved").unwrap();
            assert!(!inst.plugin_revival_pending, "{want}");
            assert_eq!(inst.archived_at, shelved.archived_at, "{want}");
            assert_eq!(inst.trashed_at, shelved.trashed_at, "{want}");
            assert_eq!(inst.last_accessed_at, shelved.last_accessed_at, "{want}");
            assert!(!state.acp_supervisor.is_running("sess-shelved").await);
        }
    }

    /// #4116: a peer that archived the stored row after the cap reservation still releases the
    /// pending mark, so the refused revival does not hold a slot.
    #[tokio::test]
    async fn turn_send_releases_the_pending_mark_when_the_stored_row_was_archived() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut resting = Instance::new("parked-owned", "/tmp/aoe-4116-plugin");
        resting.id = "sess-resting".to_string();
        resting.source_profile = "default".to_string();
        resting.view = crate::session::View::Structured;
        resting.created_by_plugin = Some("cron".to_string());
        resting.status = Status::Idle;
        let mut peer = resting.clone();
        peer.archive();
        crate::session::Storage::new_unwatched("default")
            .unwrap()
            .update(|rows, _| {
                *rows = vec![peer];
                Ok(())
            })
            .unwrap();
        let (deps, state, _dir) = test_deps_with_state(vec![resting]);

        let err = dispatch(
            &deps,
            &ctx_with(&["session.prompt"]),
            "sessions.turn.send",
            &serde_json::json!({ "session_id": "sess-resting", "text": "wake up" }),
        )
        .await
        .expect_err("a row archived on disk must not be woken");
        assert_eq!(kind(&err), "session_archived");
        let instances = state.instances.read().await;
        assert!(!instances[0].plugin_revival_pending);
    }

    /// The atomic property the pending mark exists for: a sibling revival already admitted
    /// (marked pending under the write lock, but not yet reflected in `status` since nothing
    /// in this synchronous test drives a real ACP event) must still fill the cap for a second,
    /// independent `sessions.turn.send` call, exactly as if it were already `Running`.
    #[tokio::test]
    async fn turn_send_denies_reviving_a_session_while_a_sibling_is_still_pending() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let _home = crate::session::test_support::isolate_app_dir();

        let mut prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS - 1)
            .map(|n| {
                let mut i = Instance::new("scheduled", "/tmp/aoe-4120-plugin");
                i.id = format!("sess-running-{n}");
                i.created_by_plugin = Some("cron".to_string());
                i.status = Status::Running;
                i
            })
            .collect();

        let mut already_pending = Instance::new("already-reviving", "/tmp/aoe-4120-plugin");
        already_pending.id = "sess-already-pending".to_string();
        already_pending.created_by_plugin = Some("cron".to_string());
        already_pending.status = Status::Idle;
        already_pending.plugin_revival_pending = true;
        prior.push(already_pending);

        let mut resting = Instance::new("parked-owned", "/tmp/aoe-4120-plugin");
        resting.id = "sess-resting".to_string();
        resting.view = crate::session::View::Structured;
        resting.agent_name = Some("aoe-no-such-agent-4120".to_string());
        resting.created_by_plugin = Some("cron".to_string());
        resting.status = Status::Idle;
        prior.push(resting);

        let (deps, state, _dir) = test_deps_with_state(prior);
        let ctx = ctx_with(&["session.prompt"]);

        let err = match dispatch(
            &deps,
            &ctx,
            "sessions.turn.send",
            &serde_json::json!({ "session_id": "sess-resting", "text": "wake up" }),
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("a pending-but-not-yet-counted sibling must still fill the cap"),
        };
        assert_eq!(err.code, codes::RATE_LIMITED);
        assert_eq!(kind(&err), "concurrency_limited");

        let instances = state.instances.read().await;
        let resting = instances.iter().find(|i| i.id == "sess-resting").unwrap();
        assert!(
            !resting.plugin_revival_pending,
            "a denied revival must not mark itself pending"
        );
    }

    /// A revival that never reaches `Running` (its worker never comes up, its agent doesn't
    /// exist, ...) must not hold its slot forever: with no background timer to release it,
    /// only the explicit clear on `send_turn`'s own failure stands between this and a leak.
    #[tokio::test]
    async fn turn_send_clears_the_pending_mark_when_the_revival_itself_fails() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut resting = Instance::new("parked-owned", "/tmp/aoe-4120-plugin");
        resting.id = "sess-resting".to_string();
        resting.view = crate::session::View::Structured;
        resting.agent_name = Some("aoe-no-such-agent-4120".to_string());
        resting.created_by_plugin = Some("cron".to_string());
        resting.status = Status::Idle;
        let (deps, state, _dir) = test_deps_with_state(vec![resting]);
        let ctx = ctx_with(&["session.prompt"]);

        let result = dispatch(
            &deps,
            &ctx,
            "sessions.turn.send",
            &serde_json::json!({ "session_id": "sess-resting", "text": "wake up" }),
        )
        .await;

        let instances = state.instances.read().await;
        let inst = instances.iter().find(|i| i.id == "sess-resting").unwrap();
        match result {
            Err(_) => assert!(
                !inst.plugin_revival_pending,
                "a failed revival must not hold its slot forever"
            ),
            Ok(_) => assert!(
                inst.plugin_revival_pending,
                "a successful revival stays pending until a real status lands"
            ),
        }
    }

    /// One reconciler tick with the cadence-gated passes sitting out.
    #[cfg(unix)]
    async fn reconciler_tick(state: &Arc<crate::server::AppState>) {
        let now = Some(std::time::Instant::now());
        crate::server::acp_reconciler::reconcile_acp_workers(
            state,
            &mut Default::default(),
            &mut crate::server::acp_reconciler::ReapCadence {
                idle: now,
                rate_limit: now,
                terminal_repair: now,
            },
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        )
        .await;
    }

    fn login_required_queued_message() -> String {
        format!(
            "{}; the message is queued and is delivered after the login",
            crate::session::test_support::login_required("claude")
        )
    }

    /// A message kept after a failing `before_start` hook leaves no cap reservation behind:
    /// sessions whose revival was refused for the hook do not use up the plugin's
    /// `MAX_ACTIVE_PLUGIN_SESSIONS`.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_failing_hook_leaves_no_plugin_cap_reservation() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        crate::session::config::update_config(|c| {
            c.host_hooks.before_start = vec!["exit 3".into()]
        })
        .unwrap();
        let mut rows = Vec::new();
        let ids: Vec<String> = (0..MAX_ACTIVE_PLUGIN_SESSIONS + 2)
            .map(|n| format!("sess-cap-{n}"))
            .collect();
        for id in &ids {
            let mut row = Instance::new("cap", project.to_str().unwrap());
            row.id = id.clone();
            row.created_by_plugin = Some("cron".to_string());
            row.view = crate::session::View::Structured;
            crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
            rows.push(row);
        }
        let (deps, state, _dir) = test_deps_with_state(rows);

        for id in &ids {
            let err = dispatch(
                &deps,
                &ctx_with(&["session.prompt"]),
                "sessions.turn.send",
                &serde_json::json!({ "session_id": id, "text": "hi" }),
            )
            .await
            .expect_err("the hook fails");
            assert_eq!(kind(&err), "before_start_hook_failed", "{id}: {err:?}");
        }
        assert!(
            state
                .instances
                .read()
                .await
                .iter()
                .all(|i| !i.plugin_revival_pending),
            "no reservation is left behind"
        );
    }

    /// A message for a stopped sandbox whose `before_start` hook supplies no auth route is
    /// refused for the login and kept, and the hook runs once; a hook that supplies the key
    /// lets the message through to the launch.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_plugin_message_resolves_the_before_start_hook_once() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = home.path().join("hook-runs");
        let runs = || {
            std::fs::read_to_string(&counter)
                .map(|r| r.lines().count())
                .unwrap_or(0)
        };
        for (n, (tail, refused)) in [
            ("echo FOO=bar", true),
            ("echo ANTHROPIC_API_KEY=sk-hook", false),
        ]
        .into_iter()
        .enumerate()
        {
            let hook = format!(
                "echo run >> {}; {tail}",
                crate::session::environment::shell_escape(&counter.to_string_lossy())
            );
            crate::session::config::update_config(|c| c.host_hooks.before_start = vec![hook])
                .unwrap();
            let _ = std::fs::remove_file(&counter);
            let id = format!("sess-hook-msg-{n}");
            let mut row = Instance::new("login", project.to_str().unwrap());
            row.id = id.clone();
            row.view = crate::session::View::Structured;
            crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
            let (deps, _state, _dir) = test_deps_with_state(vec![row]);

            let result = dispatch(
                &deps,
                &ctx_with(&["session.message"]),
                "sessions.message.send",
                &message_params(&id, "PR #7 was merged"),
            )
            .await;
            if refused {
                let err = result.expect_err("the hook supplies no auth route");
                assert_eq!(kind(&err), "sandbox_login_required");
                assert_eq!(
                    deps.session_service
                        .queued_prompts_snapshot(&id)
                        .await
                        .len(),
                    1
                );
            } else if let Err(err) = result {
                assert_ne!(kind(&err), "sandbox_login_required", "{err:?}");
            }
            assert_eq!(runs(), 1, "{tail}: the hook ran once");
        }

        // A hook that fails: its own error, the message kept like a login refusal.
        let hook = format!(
            "echo run >> {}; echo sekret-token >&2; exit 3",
            crate::session::environment::shell_escape(&counter.to_string_lossy())
        );
        crate::session::config::update_config(|c| c.host_hooks.before_start = vec![hook]).unwrap();
        let id = "sess-hook-msg-fails".to_string();
        let mut row = Instance::new("login", project.to_str().unwrap());
        row.id = id.clone();
        row.view = crate::session::View::Structured;
        crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
        let (deps, _state, _dir) = test_deps_with_state(vec![row]);
        let err = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params(&id, "PR #7 was merged"),
        )
        .await
        .expect_err("the hook fails");
        assert_eq!(kind(&err), "before_start_hook_failed");
        assert_eq!(err.message, "the before_start hook failed");
        assert!(
            !format!("{err:?}").contains("sekret-token"),
            "no hook output reaches the plugin error"
        );
        let queue = deps.session_service.queued_prompts_snapshot(&id).await;
        assert_eq!(queue.len(), 1, "the message is kept");
        assert_eq!(err.data.as_ref().unwrap()["queued_id"], queue[0].id);
    }

    /// A message for a stopped sandbox that needs the login is kept, and the session
    /// gets it exactly once after the login.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_queue_keeps_plugin_message() {
        use crate::acp::state::Event;
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let id = "sess-login-msg";
        let mut row = Instance::new("login", "/tmp/aoe-login-msg");
        row.id = id.to_string();
        row.view = crate::session::View::Structured;
        let shared = crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
        let (deps, state, _dir) = test_deps_with_state(vec![row]);
        let service = &deps.session_service;

        let err = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params(id, "PR #7 was merged"),
        )
        .await
        .expect_err("the login is missing");
        assert_eq!(err.code, codes::FAILED_PRECONDITION);
        assert_eq!(kind(&err), "sandbox_login_required");
        assert_eq!(err.message, login_required_queued_message());
        let queue = service.queued_prompts_snapshot(id).await;
        assert_eq!(
            queue
                .iter()
                .map(|q| (q.text.as_str(), q.origin_plugin.as_deref()))
                .collect::<Vec<_>>(),
            vec![("PR #7 was merged", Some("cron"))]
        );
        assert_eq!(err.data.as_ref().unwrap()["queued_id"], queue[0].id);
        assert!(service.sandbox_login.is_held(id));

        crate::session::test_support::write_fake_shared_credential(&shared);
        reconciler_tick(&state).await;
        assert!(!service.sandbox_login.is_held(id));
        assert!(
            !std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .is_empty(),
            "the resume after the login passed the gate"
        );

        // The fake runtime cannot start the worker; a recording fake stands in for it.
        let cmds = service
            .acp_supervisor
            .test_insert_worker_cmd_recording(id)
            .await;
        state.instances.write().await[0].status = Status::Idle;
        service.drain_queued_prompts_once(id).await;
        service.drain_queued_prompts_once(id).await;

        assert!(service.queued_prompts_snapshot(id).await.is_empty());
        let delivered: Vec<(String, Option<String>)> = state
            .acp_event_store
            .replay_from(id, 0)
            .into_iter()
            .filter_map(|(_, e)| match e {
                Event::UserPromptSent {
                    text,
                    origin_plugin,
                    ..
                } => Some((text, origin_plugin)),
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered,
            vec![("PR #7 was merged".to_string(), Some("cron".to_string()))]
        );
        service.acp_supervisor.test_flush_worker_commands(id).await;
        assert_eq!(
            cmds.lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "prompt")
                .count(),
            1
        );
    }

    /// Fills `id`'s queue to the depth cap.
    async fn fill_queue(service: &Arc<SessionService>, id: &str) {
        for n in 0..crate::server::session_service::MAX_QUEUED_PROMPTS_PER_SESSION {
            service
                .enqueue_prompt(
                    id,
                    format!("fill-{n}"),
                    format!("row {n}"),
                    Vec::new(),
                    None,
                    None,
                )
                .await
                .expect("the row is queued");
        }
    }

    fn assert_refusal_is_not_a_kept_message(err: &DispatchError, expected_kind: &str) {
        assert_eq!(kind(err), expected_kind);
        assert_ne!(kind(err), "sandbox_login_required");
        assert!(
            !err.message.contains("queued"),
            "a refusal must not claim the message is queued: {}",
            err.message
        );
        assert!(
            err.data
                .as_ref()
                .is_none_or(|d| d.get("queued_id").is_none()),
            "no queued id for a message that was not kept"
        );
    }

    /// A session held for the login keeps no more than the queue cap, whichever plugin call
    /// the message arrives by: the refusal says it was not kept.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_hold_respects_the_queue_caps_for_plugin_calls() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let cases = [
            ("sessions.message.send", "session.message"),
            ("sessions.turn.send", "session.prompt"),
        ];
        for (n, (method, capability)) in cases.into_iter().enumerate() {
            let id = format!("sess-login-cap-{n}");
            let mut row = Instance::new("login", "/tmp/aoe-login-cap");
            row.id = id.clone();
            row.view = crate::session::View::Structured;
            row.created_by_plugin = Some("cron".to_string());
            crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
            let (deps, _state, _dir) = test_deps_with_state(vec![row]);
            let service = &deps.session_service;
            fill_queue(service, &id).await;
            let params = match method {
                "sessions.turn.send" => serde_json::json!({ "session_id": id, "text": "and then" }),
                _ => message_params(&id, "and then"),
            };

            let err = dispatch(&deps, &ctx_with(&[capability]), method, &params)
                .await
                .expect_err(method);
            assert_eq!(err.code, codes::RATE_LIMITED, "{method}");
            assert_refusal_is_not_a_kept_message(&err, "queue_full");
            assert_eq!(
                service.queued_prompts_snapshot(&id).await.len(),
                crate::server::session_service::MAX_QUEUED_PROMPTS_PER_SESSION,
                "{method}"
            );
        }

        // The text cap: only a turn can be longer than a message's own limit.
        let id = "sess-login-cap-text";
        let mut row = Instance::new("login", "/tmp/aoe-login-cap-text");
        row.id = id.to_string();
        row.view = crate::session::View::Structured;
        row.created_by_plugin = Some("cron".to_string());
        crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
        let (deps, _state, _dir) = test_deps_with_state(vec![row]);
        let long = "x".repeat(crate::server::session_service::MAX_QUEUED_TEXT_BYTES + 1);
        let err = dispatch(
            &deps,
            &ctx_with(&["session.prompt"]),
            "sessions.turn.send",
            &serde_json::json!({ "session_id": id, "text": long }),
        )
        .await
        .expect_err("a turn over the text cap");
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert_refusal_is_not_a_kept_message(&err, "message_too_long");
        assert!(deps
            .session_service
            .queued_prompts_snapshot(id)
            .await
            .is_empty());
    }

    /// The queue branch of a message to a live, busy session answers a full queue the same
    /// way: refused, not kept.
    #[tokio::test]
    async fn message_send_refuses_a_busy_session_whose_queue_is_full() {
        use crate::acp::state::Event;
        use crate::acp::supervisor::BroadcastSink;
        let _home = crate::session::test_support::isolate_app_dir();
        let (deps, state, _dir, _cmds) = foreign_live_session("sess-msg-full").await;
        let sink = crate::acp::supervisor::ChannelSink {
            tx: state.acp_events_tx.clone(),
            event_store: Arc::clone(&state.acp_event_store),
            control_cache: Arc::clone(&state.acp_control_cache),
        };
        assert!(sink.publish_persisted(
            "sess-msg-full",
            1,
            &Event::UserPromptSent {
                text: "the user's turn".into(),
                attachments: Vec::new(),
                prompt_id: None,
                synthesized: false,
                origin_plugin: None,
            },
        ));
        fill_queue(&deps.session_service, "sess-msg-full").await;
        let err = dispatch(
            &deps,
            &ctx_with(&["session.message"]),
            "sessions.message.send",
            &message_params("sess-msg-full", "PR #7 was merged"),
        )
        .await
        .expect_err("the queue is full");
        assert_eq!(err.code, codes::RATE_LIMITED);
        assert_refusal_is_not_a_kept_message(&err, "queue_full");
        assert_eq!(
            deps.session_service
                .queued_prompts_snapshot("sess-msg-full")
                .await
                .len(),
            crate::server::session_service::MAX_QUEUED_PROMPTS_PER_SESSION
        );
    }

    /// A plugin-created session whose auto-spawn hit the login gate keeps its pending initial
    /// turn when a later message is refused for the same reason.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_queue_keeps_pending_initial_turn() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let cases = [
            ("sessions.message.send", "session.message", Some("cron")),
            ("sessions.turn.send", "session.prompt", None),
        ];
        for (n, (method, capability, origin_plugin)) in cases.into_iter().enumerate() {
            let id = format!("sess-login-pending-{n}");
            let mut row = Instance::new("login", "/tmp/aoe-login-pending");
            row.id = id.clone();
            row.view = crate::session::View::Structured;
            row.created_by_plugin = Some("cron".to_string());
            row.pending_initial_turn = Some(crate::session::PendingInitialTurn {
                text: "run the nightly task".to_string(),
                attachments: Vec::new(),
                synthesized: false,
            });
            crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
            let (deps, state, _dir) = test_deps_with_state(vec![row]);
            let service = &deps.session_service;
            // The refused auto-spawn after the create.
            service.sandbox_login.hold(
                &id,
                crate::server::sandbox_login::LaunchOrigin::Unattended,
                &crate::session::test_support::login_required("claude"),
            );

            let params = match method {
                "sessions.turn.send" => serde_json::json!({ "session_id": id, "text": "and then" }),
                _ => message_params(&id, "and then"),
            };
            let err = dispatch(&deps, &ctx_with(&[capability]), method, &params)
                .await
                .expect_err(method);
            assert_eq!(kind(&err), "sandbox_login_required", "{method}");
            assert_eq!(err.message, login_required_queued_message(), "{method}");

            let instances = state.instances.read().await;
            assert_eq!(
                instances[0]
                    .pending_initial_turn
                    .as_ref()
                    .map(|t| t.text.as_str()),
                Some("run the nightly task"),
                "{method}"
            );
            drop(instances);
            let queue = service.queued_prompts_snapshot(&id).await;
            assert_eq!(
                queue
                    .iter()
                    .map(|q| (q.text.as_str(), q.origin_plugin.as_deref()))
                    .collect::<Vec<_>>(),
                vec![("and then", origin_plugin)],
                "{method}"
            );
            assert_eq!(
                err.data.as_ref().unwrap()["queued_id"],
                queue[0].id,
                "{method}"
            );
        }
    }

    /// A turn kept for the login keeps its cap reservation: it revives the session after the
    /// login, so further revivals past the cap are still refused.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_queue_keeps_the_revival_reservation() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let rows: Vec<Instance> = (0..=MAX_ACTIVE_PLUGIN_SESSIONS)
            .map(|n| {
                let mut row = Instance::new("login", "/tmp/aoe-login-cap");
                row.id = format!("sess-login-cap-{n}");
                row.view = crate::session::View::Structured;
                row.created_by_plugin = Some("cron".to_string());
                row.mark_idle_dormant();
                crate::session::test_support::sandbox_needing_login(home.path(), &mut row);
                row
            })
            .collect();
        let (deps, state, _dir) = test_deps_with_state(rows);
        let ctx = ctx_with(&["session.prompt"]);

        for n in 0..=MAX_ACTIVE_PLUGIN_SESSIONS {
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": format!("sess-login-cap-{n}"), "text": "go" }),
            )
            .await
            .expect_err("no turn starts");
            let want = if n < MAX_ACTIVE_PLUGIN_SESSIONS {
                "sandbox_login_required"
            } else {
                "concurrency_limited"
            };
            assert_eq!(kind(&err), want, "turn {n}");
        }
        let reserved = state
            .instances
            .read()
            .await
            .iter()
            .filter(|i| i.plugin_revival_pending)
            .count();
        assert_eq!(reserved, MAX_ACTIVE_PLUGIN_SESSIONS);
    }
}

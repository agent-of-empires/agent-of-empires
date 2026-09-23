//! Async worker RPC handlers for the session-driving plugin API (#2897):

use std::sync::Arc;

use serde_json::Value;

use aoe_plugin_api::acp::{
    AcpAgentCapability, AcpCapabilitiesResponse, AcpModeCapability, AcpModelCapability,
    AcpThinkingCapability, ApprovalClass, CatalogStatus,
};
use aoe_plugin_api::session::{SessionsCreateRequest, SessionsCreateResponse, TurnSendRequest};

use crate::acp::option_catalog::{AgentOptionEntry, OptionCatalog};
use crate::acp::state::ConfigOptionCategory;
use crate::plugin::automation_policy::{classify_mode, AutomationPolicy, ModeDecision};
use crate::plugin::host_api::{DispatchError, PluginRpcContext};
use crate::plugin::protocol::codes;
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

pub struct SessionRpcDeps {
    pub session_service: Arc<SessionService>,
    pub policy: Arc<AutomationPolicy>,
    pub profile: String,
}

pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        "acp.capabilities.get"
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

    let active_sessions = {
        let instances = deps.session_service.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.created_by_plugin.as_deref() == Some(plugin_id)
                    && !i.is_archived()
                    && !i.is_snoozed()
                    && !i.is_trashed()
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
        let woke_idle_dormant = deps
            .session_service
            .touch_and_wake_on_prompt(&req.session_id, false)
            .await
            .idle_dormant();
        let dispatch = deps
            .session_service
            .prompt_dispatch_under_submission(&req.session_id, woke_idle_dormant)
            .await;
        if let crate::acp::dispatch::PromptDispatch::Queued { reason } = dispatch {
            if !matches!(reason, crate::acp::dispatch::QueueReason::WorkerDown) {
                return Err(DispatchError::with_kind(
                    codes::SERVICE_UNAVAILABLE,
                    "agent_busy",
                    "the session's agent is mid-turn; retry when it finishes",
                ));
            }
        }
        deps.session_service
            .send_turn(
                &caller,
                &req.session_id,
                SendTurnRequest {
                    text: &req.text,
                    attachments: &[],
                    woke_idle_dormant,
                    prompt_id: None,
                    synthesized: false,
                    no_revive: false,
                },
            )
            .await
            .map_err(map_send_error)
    }
    .await;

    deps.policy.audit(
        &plugin_id,
        serde_json::json!({
            "op": "sessions.turn.send",
            "session": req.session_id,
            "decision": if result.is_ok() { "ok" } else { "denied" },
            "kind": result.as_ref().err().and_then(|e| {
                e.data.as_ref().and_then(|d| d.get("kind")).cloned()
            }),
        }),
    );
    result?;
    Ok(serde_json::json!({}))
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
        SendTurnError::Send(e) => DispatchError::internal(format!("prompt forward failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::automation_policy::AutomationPolicy;
    use crate::session::Instance;

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
    }

    #[tokio::test]
    async fn unattended_mode_requires_the_distinct_grant() {
        let (deps, _dir) = test_deps(Vec::new());
        let params = serde_json::json!({
            "agent_id": "claude",
            "project_path": "/tmp",
            "mode_id": "bypassPermissions",
        });
        let ctx = ctx_with(&["session.create"]);
        let err = dispatch(&deps, &ctx, "sessions.create", &params)
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
    async fn create_refuses_a_model_off_the_profile_pin() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        write_app_config(
            "[acp.acp_defaults.claude]\nmodel = \"claude-pinned\"\npin_model = true\n",
        );
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.create"]);

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
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn create_reads_the_pin_of_the_agent_a_wrapper_spawns() {
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
    async fn probe_unknown_agent_is_noop_and_returns_catalog() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["acp.capabilities.probe"]);
        let out = dispatch(
            &deps,
            &ctx,
            "acp.capabilities.probe",
            &serde_json::json!({ "agent_id": "definitely-not-an-agent-xyz" }),
        )
        .await
        .expect("probe returns the capability catalog");
        assert!(out.get("agents").is_some());
    }

    #[tokio::test]
    async fn create_at_concurrency_limit_denies_a_new_key() {
        use crate::plugin::automation_policy::MAX_ACTIVE_PLUGIN_SESSIONS;
        let prior: Vec<Instance> = (0..MAX_ACTIVE_PLUGIN_SESSIONS)
            .map(|n| {
                let mut i = Instance::new("scheduled", "/tmp/aoe-2897-project");
                i.id = format!("sess-{n}");
                i.created_by_plugin = Some("cron".to_string());
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
    async fn turn_send_maps_ownership_and_missing_session() {
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
            ("archived", |i| i.archived_at = Some(chrono::Utc::now())),
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
    async fn turn_send_does_not_grow_the_lock_registry_for_nonexistent_sessions() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.prompt"]);

        for i in 0..5 {
            let err = dispatch(
                &deps,
                &ctx,
                "sessions.turn.send",
                &serde_json::json!({ "session_id": format!("sess-gone-{i}"), "text": "hi" }),
            )
            .await
            .expect_err("must be refused");
            assert_eq!(kind(&err), "session_not_found");
        }

        assert_eq!(
            deps.session_service.prompt_locks_len().await,
            0,
            "an id that was never admitted must not leave a lock-registry entry behind"
        );
    }
}

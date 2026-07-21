//! Async worker RPC handlers for the session-driving plugin API (#2897):
//! `acp.capabilities.get`, `sessions.create`, `sessions.turn.send`.
//!
//! These run on the async runtime (unlike the synchronous
//! [`crate::plugin::host_api::dispatch`]) because they call into the shared
//! `SessionService`. Authorization layers, in order: capability grants
//! (connection context, never payload), host-side approval classification
//! (`session.unattended` for unattended modes), automation policy limits,
//! and the service's own invariants (repo trust fail-closed, plugin
//! ownership on turn delivery, idempotency).

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
    CreateIdempotencyProbe, IdempotencyConflict, SendTurnError, SessionCaller, SessionService,
};
use crate::server::session_spawn::StructuredSessionSpec;

/// Upper bound on `extra_project_paths` per create, so one plugin call cannot
/// trigger an unbounded chain of blocking `canonicalize` calls.
const MAX_EXTRA_PROJECT_PATHS: usize = 16;

const CAP_ACP_CAPABILITIES_READ: &str = "acp.capabilities.read";
const CAP_ACP_CAPABILITIES_PROBE: &str = "acp.capabilities.probe";
const CAP_SESSION_CREATE: &str = "session.create";
const CAP_SESSION_PROMPT: &str = "session.prompt";
const CAP_SESSION_UNATTENDED: &str = "session.unattended";

/// Everything the session RPCs need, injected into the plugin host at
/// construction (before any worker launches).
pub struct SessionRpcDeps {
    pub session_service: Arc<SessionService>,
    pub policy: Arc<AutomationPolicy>,
    /// The serving profile new sessions are created under.
    pub profile: String,
}

/// Whether `method` belongs to this module's async dispatch.
pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        "acp.capabilities.get"
            | "acp.capabilities.probe"
            | "sessions.create"
            | "sessions.turn.send"
    )
}

/// The base capability a session method requires. Exposed so the host can
/// authorize before consulting the session dependencies, keeping the authz
/// result identical whether or not the service happens to be wired up.
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

/// Merge the static agent registry with the last advertised option catalog
/// into the stable public DTO. Pure reads; never launches an agent.
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
                                // Advertised modes always classify; fail
                                // closed if that invariant ever breaks.
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
                // The registry has no display metadata; the id doubles as
                // the display name until it grows one.
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

/// `acp.capabilities.probe`: populate the option catalog for one agent (or every
/// currently-undiscovered registry agent when no `agent_id` is given) via a
/// handshake-only ACP probe, then return the same shape as
/// `acp.capabilities.get`. Each probe degrades to a no-op on failure, so a
/// missing adapter or an agent that needs credentials the daemon lacks simply
/// stays `Undiscovered` instead of erroring the whole call.
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

    // Agent must be a registry agent or one the catalog has observed.
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

    // Host-side approval classification; the plugin cannot self-label.
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

    // Model must be advertised when the catalog is discovered; with an
    // undiscovered catalog it passes through and the adapter arbitrates.
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

    // Resolve the project selection into (path, extra_repo_paths, scratch).
    // No project -> a scratch session (no repo, hence no trust anchor). One or
    // more projects -> the first is the trust-checked primary repo and the rest
    // are extra repos. Canonicalize immediately before the trust-checked spawn;
    // a dangling path is the caller's error. Repo trust itself is enforced
    // inside the service, fail-closed for plugin callers.
    let primary = req
        .project_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let (project_path, extra_repo_paths, scratch) = match primary {
        None => {
            // A scratch session has no repo, so extra repos are meaningless and
            // the builder refuses the combination; reject early and clearly.
            if req.extra_project_paths.iter().any(|p| !p.trim().is_empty()) {
                return Err(DispatchError::invalid_params(
                    "extra_project_paths requires a project_path; a scratch session takes no extra repos",
                ));
            }
            (String::new(), Vec::new(), true)
        }
        Some(primary) => {
            // Cap the extras before any blocking work so one call cannot tie up
            // a runtime worker with a long canonicalization chain.
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
            // Filesystem canonicalization is blocking; run it off the async
            // runtime rather than stalling a worker thread.
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
        sandbox: false,
        sandbox_image: None,
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        extra_repo_paths,
        scratch,
        // The service forces this to Some(false) for plugin callers; set
        // explicitly anyway so the intent is local.
        trust_hooks: Some(false),
        custom_instruction: None,
        profile: deps.profile.clone(),
        created_by_plugin: None,
        plugin_create_idempotency: None,
        // Set here (not just inside the service) so the idempotency probe below
        // hashes the same payload the create will.
        pending_initial_turn: req.initial_turn.as_ref().map(|t| t.text.clone()),
        acp_mode_id: req.mode_id.clone(),
        view: crate::session::View::Structured,
        agent_name: None,
        agent_model: req.model_id.clone(),
        agent_effort: None,
        import_acp_session_id: None,
        fork_seed: None,
    };

    // Resolve an idempotent replay/conflict BEFORE charging admission, so a
    // retry after a lost response returns the prior result without consuming
    // rate or concurrency capacity (#2897). A brand-new key falls through to
    // the reservation and create below.
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
    // Held until the create resolves so concurrent different-key creates
    // cannot overshoot the cap.
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
        deps.session_service
            .send_turn(
                &SessionCaller::Plugin {
                    plugin_id: plugin_id.clone(),
                },
                &req.session_id,
                &req.text,
                &[],
                false,
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
        let dir = tempfile::tempdir().expect("tempdir");
        let session_service = crate::server::test_support::build_test_app_state(prior)
            .session_service
            .clone();
        let policy =
            Arc::new(AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("policy"));
        (
            Arc::new(SessionRpcDeps {
                session_service,
                policy,
                profile: "test".to_string(),
            }),
            dir,
        )
    }

    fn kind(e: &DispatchError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// Every method refuses a caller missing its gating capability, before
    /// touching any state.
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
        // The wrong capability does not substitute for the right one.
        let wrong = ctx_with(&["session.prompt"]);
        let err = dispatch(&deps, &wrong, "sessions.create", &serde_json::json!({}))
            .await
            .expect_err("session.prompt must not grant sessions.create");
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    /// An unattended-classified mode needs the distinct session.unattended
    /// grant; session.create alone is refused with the stable policy kind.
    /// Uses a trusted-table bypass id so the decision is catalog-independent.
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

    /// A payload smuggling an unknown field (a would-be bypass flag) is
    /// rejected at decode, before any capability-gated work.
    #[tokio::test]
    async fn create_rejects_unknown_payload_fields() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.create"]);
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({
                "agent_id": "claude",
                "project_path": "/tmp",
                "allow_untrusted": true,
            }),
        )
        .await
        .expect_err("unknown fields must be rejected");
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    /// The probe RPC decodes params strictly: an unknown field is a client
    /// error, refused before any spawn work.
    #[tokio::test]
    async fn probe_rejects_unknown_params() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["acp.capabilities.probe"]);
        let err = dispatch(
            &deps,
            &ctx,
            "acp.capabilities.probe",
            &serde_json::json!({ "bogus": 1 }),
        )
        .await
        .expect_err("unknown probe param must be rejected");
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    /// A scratch create (no project_path) may not carry extra repos: the
    /// session builder refuses that combination, so the RPC rejects it up front
    /// with a clear invalid-params error, before any spawn.
    #[tokio::test]
    async fn scratch_with_extra_repos_is_rejected() {
        let (deps, _dir) = test_deps(Vec::new());
        let ctx = ctx_with(&["session.create"]);
        let err = dispatch(
            &deps,
            &ctx,
            "sessions.create",
            &serde_json::json!({
                "agent_id": "claude",
                "extra_project_paths": ["/tmp"],
            }),
        )
        .await
        .expect_err("scratch + extra repos must be refused");
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    /// A registry-unknown `agent_id` never spawns anything (the probe bails on
    /// an unknown agent), so this stays hermetic while still exercising the RPC
    /// end to end and confirming it returns the capability catalog shape.
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

    /// A brand-new create at the active-session limit is denied with the stable
    /// concurrency kind. The idempotency probe runs before admission (see
    /// `admit_and_create`), so an idempotent retry replays instead of hitting
    /// this path; the replay/conflict/new resolution itself is unit-tested in
    /// `server::session_service::tests::probe_resolves_replay_conflict_and_new`.
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
        // "claude" with no mode classifies Interactive (reviewed adapter), so no
        // unattended grant is needed and the request reaches the limit check.
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

    /// turn.send maps the service's ownership and existence denials to the
    /// stable error kinds.
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
}

//! REST write side of structured view sessions; the structured view WebSocket
//! carries the read side.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::acp::acp_client::AcpError;
use crate::acp::supervisor::{SpawnRequest, SupervisorError};
use crate::server::AppState;

use super::{cityhall_block, read_only_block};

mod attachments;
mod config;
mod history;
mod install;
mod prompt;
mod view;
mod worker;

pub(crate) use attachments::{sniff_image_mime, validate_attachments};
pub use config::{acp_set_config_option, acp_set_mode};
pub(crate) use history::read_log_tail;
pub use history::{
    acp_context_primer, acp_files, acp_replay, acp_worker_log, list_claude_sessions,
};
pub use install::install_agent;
pub use prompt::{
    acp_attachment, acp_cancel, acp_force_end_turn, acp_prompt, acp_prompt_diff_comments,
    resolve_approval, resolve_elicitation,
};
pub use view::{acp_disable, acp_enable};
pub use worker::{get_option_catalog, list_acp_agents, shutdown_acp, spawn_acp, switch_acp_agent};

/// Startup-error banner text for a failed detached structured-view spawn.
/// `CapacityFull` is surfaced verbatim so the UI shows the capacity banner.
pub(crate) fn structured_spawn_error_message(err: &SupervisorError, agent: &str) -> String {
    match err {
        SupervisorError::CapacityFull { .. } => err.to_string(),
        _ => format!("Failed to start structured view agent {agent:?}: {err}"),
    }
}

fn session_not_found() -> Response {
    (StatusCode::NOT_FOUND, "session not found").into_response()
}

/// Plain-text status mapping for a `SupervisorError`. The dashboard matches
/// body prefixes (`worker_not_ready`, `rate_limited`, `spawn_cancelled`).
fn supervisor_error_response(context: &str, err: &SupervisorError) -> Response {
    let (status, body) = match err {
        SupervisorError::UnknownSession(_) => (
            StatusCode::NOT_FOUND,
            "session has no running structured view".to_string(),
        ),
        SupervisorError::UnknownAgent(name) => (
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {name}"),
        ),
        // 403: the agent exists but the operator's policy refuses it.
        SupervisorError::AgentNotAllowed(_) => (StatusCode::FORBIDDEN, err.to_string()),
        SupervisorError::AlreadyRunning(_) => (
            StatusCode::CONFLICT,
            "structured view worker already running for session".to_string(),
        ),
        SupervisorError::CapacityFull { .. } | SupervisorError::TeardownPending(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        // The worker is mid-restart (e.g. a force stop); the client re-queues.
        SupervisorError::Acp(AcpError::AgentExited | AcpError::NotRunning) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_not_ready: {context}: {err}"),
        ),
        SupervisorError::Acp(AcpError::RateLimited(_)) => (
            StatusCode::TOO_MANY_REQUESTS,
            format!("rate_limited: {context}: {err}"),
        ),
        // A stop superseded the spawn.
        SupervisorError::SpawnCancelled(_) => (
            StatusCode::CONFLICT,
            format!("spawn_cancelled: {context}: {err}"),
        ),
        SupervisorError::Acp(_) | SupervisorError::InvalidAgentCommand(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{context}: {err}"),
        ),
    };
    (status, body).into_response()
}

/// A spawn request carrying the instance's persisted worker settings.
fn spawn_request_for(
    instance: &crate::session::Instance,
    agent: String,
    sandbox_info: Option<crate::session::SandboxInfo>,
) -> SpawnRequest {
    SpawnRequest {
        session_id: instance.id.clone(),
        agent,
        tool: instance.tool.clone(),
        cwd: PathBuf::from(&instance.project_path),
        additional_dirs: vec![],
        provider_env: vec![],
        model: instance.agent_model.clone(),
        effort: instance.acp_effort.clone(),
        effort_explicit: instance.acp_effort.is_some(),
        stored_acp_session_id: instance.acp_session_id.clone(),
        fork_from: instance.fork_pending.clone(),
        sandbox_info,
        // Passed even without sandboxing so agent_acp_cmd and worker env
        // resolve from the session's profile.
        source_profile: Some(instance.source_profile.clone()),
        yolo_mode: instance.yolo_mode,
        acp_mode_id: instance.acp_mode_id.clone(),
        agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
            &instance.tool,
            &instance.command,
        ),
        seed_history_replay: instance.import_pending == Some(true),
    }
}

async fn pick_agent(
    state: &AppState,
    instance: &crate::session::Instance,
    explicit: Option<&str>,
) -> String {
    state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            explicit,
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_exited_maps_to_the_retryable_status_not_a_fault() {
        let cases = [
            (
                SupervisorError::Acp(AcpError::AgentExited),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SupervisorError::Acp(AcpError::NotRunning),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SupervisorError::Acp(AcpError::Protocol("bad frame".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                SupervisorError::UnknownSession("s-1".into()),
                StatusCode::NOT_FOUND,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(
                supervisor_error_response("prompt failed", &err).status(),
                expected,
                "{err}"
            );
        }

        // The composer re-queues on a 503 body starting with this prefix.
        let body = supervisor_error_response(
            "prompt failed",
            &SupervisorError::Acp(AcpError::AgentExited),
        )
        .into_body();
        let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).starts_with("worker_not_ready"));
    }

    #[tokio::test]
    async fn missing_session_does_not_create_instance_lock() {
        for which in ["spawn", "enable", "disable"] {
            let state = crate::server::test_support::build_test_app_state(Vec::new());
            let (state_arg, id) = (State(state.clone()), Path("missing".to_string()));
            let response = match which {
                "spawn" => spawn_acp(
                    state_arg,
                    id,
                    Ok(Json(worker::SpawnAcpRequest {
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response(),
                "enable" => acp_enable(state_arg, id).await.into_response(),
                _ => acp_disable(state_arg, id).await.into_response(),
            };
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{which}");
            assert!(state.instance_locks.read().await.is_empty(), "{which}");
        }
    }

    /// #3650: endpoints that tear the worker down wait for an in-flight
    /// submission, or `send_turn` would respawn the worker they stop.
    #[tokio::test]
    async fn worker_stopping_acp_endpoints_wait_for_an_in_flight_submission() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        use std::time::Duration;

        for which in ["shutdown", "switch", "disable"] {
            let mut inst = crate::session::Instance::new("acp-3650", "/tmp/aoe-3650-acp");
            inst.id = format!("sess-3650-acp-{which}");
            inst.view = crate::session::View::Structured;
            inst.status = crate::session::Status::Idle;
            inst.acp_load_session_capable = Some(true);
            let id = inst.id.clone();
            let state = crate::server::test_support::build_test_app_state(vec![inst]);

            let delivering = state.session_service.prompt_submission(&id).await;
            let mut claims = state.session_service.watch_submission_claims();
            let handler = {
                let (state, id) = (State(Arc::clone(&state)), Path(id.clone()));
                async move {
                    match which {
                        "shutdown" => shutdown_acp(state, id).await.into_response(),
                        "switch" => switch_acp_agent(
                            state,
                            id,
                            Json(crate::acp::protocol::SwitchAgentRequest {
                                target: "codex".to_string(),
                                model: None,
                                reason: None,
                            }),
                        )
                        .await
                        .into_response(),
                        _ => acp_disable(state, id).await.into_response(),
                    }
                }
            };
            tokio::pin!(handler);
            assert!(futures_util::poll!(&mut handler).is_pending(), "{which}");
            assert_eq!(claims.try_recv().expect("contender reached claim"), id);

            drop(delivering);
            tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"));
            if which == "disable" {
                let instances = state.instances.read().await;
                let inst = instances.iter().find(|inst| inst.id == id).unwrap();
                assert_eq!(inst.acp_load_session_capable, None);
            }
        }
    }
}

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

use super::cityhall_block;
use super::read_only_block;

mod attachments;
mod config;
mod history;
mod install;
mod prompt;
mod view;
mod worker;

pub(crate) use attachments::{sniff_image_mime, validate_attachments};
pub use config::{acp_set_config_option, acp_set_mode};
pub use history::{
    acp_context_primer, acp_files, acp_replay, acp_task_output, acp_worker_log,
    list_importable_sessions,
};
pub(crate) use history::{importable_sessions, list_error_response, read_log_tail};
pub use install::{agent_install_info, install_agent};
pub use prompt::{
    acp_attachment, acp_cancel, acp_cancel_subagent, acp_force_end_turn, acp_prompt,
    acp_prompt_diff_comments, resolve_approval, resolve_elicitation,
};
pub(crate) use prompt::{submit_prompt, PromptSubmission, PromptSubmitError, PromptSubmitted};
pub use view::{acp_disable, acp_enable, acp_terminal_check};
pub(crate) use view::{
    publish_startup_error_if_current, publish_startup_error_with_cause_if_current,
    recover_pending_terminal_handoffs, HandoffRecovery,
};
pub(in crate::server::api) use worker::rate_limit_resume_probe;
pub use worker::{
    get_option_catalog, list_acp_agents, restart_acp, shutdown_acp, spawn_acp, switch_acp_agent,
    switch_acp_provider,
};

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
        SupervisorError::RestartInProgress(_) => {
            (StatusCode::CONFLICT, format!("restart_in_progress: {err}"))
        }
        SupervisorError::Superseded(_) | SupervisorError::StopSuperseded(_) => {
            (StatusCode::CONFLICT, err.to_string())
        }
        SupervisorError::CapacityFull { .. }
        | SupervisorError::TeardownPending(_)
        | SupervisorError::PurgeInProgress(_)
        | SupervisorError::EventStoreUnavailable(_)
        | SupervisorError::NativeStoreUnavailable => {
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
        SupervisorError::Blocked(blocked) => {
            return crate::server::api::start_blocked_response(*blocked)
        }
        SupervisorError::SessionGone(_) => return session_not_found(),
        SupervisorError::Acp(_) | SupervisorError::InvalidAgentCommand(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{context}: {err}"),
        ),
    };
    (status, body).into_response()
}

/// Whether a launch error is a failed storage write rather than one of the
/// typed refusals `launch_error_response` maps.
fn is_store_write_failure(error: &anyhow::Error) -> bool {
    !(error.is::<crate::session::StartBlocked>()
        || error.is::<crate::session::SessionGone>()
        || error.is::<crate::session::LifecycleReservationError>()
        || error.is::<crate::session::NativeStoreUnavailable>())
}

/// A durable write failed after the worker had taken the change. Stop the worker
/// and have it respawn from the stored row; writing the old value back could
/// overwrite a newer edit.
async fn persist_failure_response(
    state: &AppState,
    id: &str,
    what: &str,
    err: &anyhow::Error,
) -> Response {
    tracing::error!(
        target: "http.api.acp",
        session = %id,
        "failed to persist {what}: {err:#}"
    );
    // `shutdown_and_wait` can return Ok at its deadline without a confirmed stop.
    let shutdown_ok = state
        .acp_supervisor
        .shutdown_and_wait(id, std::time::Duration::from_secs(5))
        .await
        .is_ok();
    let worker_state = state.acp_supervisor.worker_state(id).await;
    let stopped = shutdown_ok && worker_state == crate::daemon::AcpWorkerState::Absent;
    // The reconciler skips a session it already attempted unless asked, and a
    // teardown still in flight settles after this returns.
    if stopped
        || matches!(
            worker_state,
            crate::daemon::AcpWorkerState::Resuming | crate::daemon::AcpWorkerState::Stopping
        )
    {
        state.acp_supervisor.request_respawn(id);
    }
    let outcome = if stopped {
        "the worker was stopped and the stored value is unchanged; retry"
    } else {
        "the worker may still be running the unsaved change"
    };
    super::api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "persist_failed",
        format!("failed to save the {what} ({err:#}); {outcome}"),
    )
}

/// A spawn request carrying the instance's persisted worker settings.
pub(crate) fn spawn_request_for(
    instance: &crate::session::Instance,
    agent: String,
    sandbox_info: Option<crate::session::SandboxInfo>,
) -> SpawnRequest {
    SpawnRequest {
        launch_admission: None,
        expected_lifecycle_generation: instance.lifecycle_generation,
        session_id: instance.id.clone(),
        agent,
        tool: instance.tool.clone(),
        cwd: PathBuf::from(&instance.project_path),
        config_path: instance.repo_config_path().to_path_buf(),
        additional_dirs: vec![],
        provider_env: vec![],
        provider: instance.agent_provider.clone(),
        model: instance.agent_model.clone(),
        effort: instance.acp_effort.clone(),
        effort_explicit: instance.acp_effort.is_some(),
        model_config: instance.acp_model_config.clone(),
        stored_acp_session_id: instance.acp_session_id.clone(),
        fork_from: instance.fork_pending.clone(),
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Persisted,
        sandbox_info,
        workspace_info: instance.workspace_info.clone(),
        // Passed even without sandboxing so agent_acp_cmd and worker env
        // resolve from the session's profile.
        source_profile: Some(instance.source_profile.clone()),
        yolo_mode: instance.yolo_mode,
        acp_mode_id: instance.acp_mode_id.clone(),
        agent_command_override: crate::acp::supervisor::command_override_for_spawn(
            &instance.tool,
            &instance.command,
        ),
        seed_history_replay: instance.import_pending == Some(true),
        prior_conversation: false,
        claude_store_pin: instance.selected_claude_store_pin(),
        proxy: None,
    }
}

async fn launch_store_for(
    state: &Arc<AppState>,
    instance: &crate::session::Instance,
    exclusion: crate::acp::sandbox::LaunchExclusion,
) -> Result<
    (
        Arc<crate::server::session_store::NativeSessionStore>,
        crate::acp::sandbox::LaunchExclusion,
    ),
    Response,
> {
    crate::server::session_store::NativeSessionStore::open_for_launch(
        state.clone(),
        instance,
        exclusion,
    )
    .await
    .map_err(|error| launch_error_response("session launch preflight", &error))
}

fn launch_error_response(context: &str, error: &anyhow::Error) -> Response {
    if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
        return crate::server::api::start_blocked_response(*blocked);
    }
    if let Some(login) = error.downcast_ref::<crate::session::SandboxLoginRequired>() {
        return crate::server::api::sandbox_login_required_response(login);
    }
    if let Some(unsupported) = error.downcast_ref::<crate::session::SandboxLoginUnsupported>() {
        return crate::server::api::sandbox_login_unsupported_response(unsupported);
    }
    if let Some(hook) = error.downcast_ref::<crate::session::BeforeStartHookError>() {
        // The hook's own failure, in fixed words; a switch queues nothing.
        return crate::server::api::before_start_hook_failed_response(hook);
    }
    if error.is::<crate::session::SessionGone>() {
        return session_not_found();
    }
    if error.is::<crate::session::LifecycleReservationError>() {
        return (
            StatusCode::CONFLICT,
            format!("spawn_cancelled: {context}: {error}"),
        )
            .into_response();
    }
    if error.is::<crate::session::NativeStoreUnavailable>() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{context}: {error}"),
    )
        .into_response()
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
            instance.repo_config_path(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_handoff_test_state(
        prior: Vec<crate::session::Instance>,
        app_dir: &std::path::Path,
    ) -> Arc<AppState> {
        let db_path = app_dir.join(format!("acp-events-{}.db", prior[0].id));
        let event_store = Arc::new(
            crate::acp::event_store::EventStore::open(&db_path, 100).expect("event store"),
        );
        crate::server::test_support::build_test_app_state_impl_with_event_store(
            prior,
            Vec::new(),
            Vec::new(),
            None,
            Some(event_store),
            |_| {},
            |supervisor| supervisor,
        )
    }

    /// Shared by the agent switch and the selector picks: names the operation,
    /// stops the worker, and answers 500.
    #[tokio::test]
    async fn persist_failure_names_the_operation_and_stops_the_worker() {
        let state = crate::server::test_support::build_test_app_state(vec![]);
        state.acp_supervisor.test_insert_worker("sess").await;

        let resp = persist_failure_response(
            &state,
            "sess",
            "agent switch",
            &anyhow::anyhow!("disk full"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("persist_failed") && body.contains("agent switch"));
        assert!(body.contains("disk full") && body.contains("worker was stopped"));
        assert_eq!(
            state.acp_supervisor.worker_state("sess").await,
            crate::daemon::AcpWorkerState::Absent
        );
    }

    #[test]
    fn spawn_request_for_carries_the_workspace_layout() {
        let instance = crate::server::test_support::workspace_instance(
            "ws-spawn-request",
            std::path::Path::new("/tmp/aoe-ws-spawn-request"),
        );
        let req = spawn_request_for(&instance, "claude".into(), None);
        crate::server::test_support::assert_carries_workspace(
            req.workspace_info.as_ref(),
            &instance,
            "spawn_request_for",
        );
        assert_eq!(req.cwd, PathBuf::from(&instance.project_path));
    }

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
                _ => acp_disable(state_arg, id, Default::default())
                    .await
                    .into_response(),
            };
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{which}");
            assert!(state.instance_locks.read().await.is_empty(), "{which}");
        }
    }

    /// #4116: a peer can archive or purge the stored row after the handler's memory check; the
    /// spawn rechecks that row and refuses.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_refuses_a_row_archived_or_purged_on_disk() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut inst = crate::session::Instance::new("acp-4116", "/tmp/aoe-4116-acp");
        inst.source_profile = "default".into();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let id = inst.id.clone();
        for (stored_row, want_status, want_code) in [
            (true, StatusCode::CONFLICT, "session_archived"),
            (false, StatusCode::NOT_FOUND, ""),
        ] {
            let mut peer = inst.clone();
            peer.archive();
            crate::session::Storage::new_unwatched(&inst.source_profile)
                .unwrap()
                .update(|rows, _| {
                    *rows = if stored_row { vec![peer] } else { Vec::new() };
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
            let response = spawn_acp(
                State(state.clone()),
                Path(id.clone()),
                Ok(Json(worker::SpawnAcpRequest {
                    agent: None,
                    model: None,
                    additional_dirs: Vec::new(),
                    provider_env: Vec::new(),
                })),
            )
            .await
            .into_response();
            assert_eq!(response.status(), want_status, "stored_row={stored_row}");
            if stored_row {
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], want_code);
            }
            assert!(!state.acp_supervisor.is_running(&id).await);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn launch_admission_progresses_with_a_group_reader_and_queued_namespace_writer() {
        use crate::server::test_support as support;
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut instance =
            crate::session::Instance::new("namespace-launch", project.path().to_str().unwrap());
        instance.source_profile = "default".into();
        instance.view = crate::session::View::Structured;
        let id = instance.id.clone();
        support::seed_instances_on_disk_for_test("default", vec![instance.clone()]);
        let (launcher, launches) = support::counting_failing_launcher();
        let state = support::build_test_app_state_with_launcher(vec![instance], launcher);
        support::refresh_canonical_metadata_for_test(&state).await;
        let held = state.instance_lock(&id).await.lock_owned().await;
        let mut launch = Box::pin(spawn_acp(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(worker::SpawnAcpRequest {
                agent: Some("codex".into()),
                model: None,
                additional_dirs: Vec::new(),
                provider_env: Vec::new(),
            })),
        ));
        assert!(futures_util::poll!(launch.as_mut()).is_pending());
        let mut group = Box::pin(crate::server::api::sessions::update_session_group(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(crate::daemon::UpdateGroupBody {
                group: "moved".into(),
            })),
        ));
        assert!(futures_util::poll!(group.as_mut()).is_pending());
        let writer = async {
            drop(state.profile_namespace.write().await);
        };
        tokio::pin!(writer);
        assert!(futures_util::poll!(writer.as_mut()).is_pending());
        drop(held);
        let (launch, group, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(launch, group, writer)
        })
        .await
        .expect("launch must not reacquire namespace while retaining instance exclusion");
        assert_eq!(
            launch.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let group = group.into_response();
        let status = group.status();
        let body = axum::body::to_bytes(group.into_body(), 4096).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(state.instances.read().await[0].group_path, "moved");
    }

    /// Whether `id`'s Retry owner is rolled back, `None` with no owner. A failed Retry keeps its
    /// rolled-back owner for the new worker's startup error.
    fn rolled_back(state: &AppState, id: &str) -> Option<bool> {
        state
            .retry_starting
            .lock()
            .unwrap()
            .get(id)
            .map(crate::server::acp_events::RetryAttempt::is_rolled_back)
    }

    fn retry_request(
    ) -> Result<Json<worker::SpawnAcpRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(worker::SpawnAcpRequest {
            agent: Some("codex".into()),
            model: None,
            additional_dirs: Vec::new(),
            provider_env: Vec::new(),
        }))
    }

    async fn errored_retry_state(
        project: &std::path::Path,
        id: &str,
    ) -> Arc<crate::server::state::AppState> {
        let (launcher, _launches) = crate::server::test_support::counting_failing_launcher();
        errored_retry_state_with(project, id, launcher, None).await
    }

    async fn errored_retry_state_with(
        project: &std::path::Path,
        id: &str,
        launcher: crate::acp::supervisor::Launcher,
        acp_mode_id: Option<&str>,
    ) -> Arc<crate::server::state::AppState> {
        use crate::server::test_support as support;
        let mut inst = crate::session::Instance::new("acp-5671", project.to_str().unwrap());
        inst.id = id.into();
        inst.acp_mode_id = acp_mode_id.map(str::to_owned);
        inst.source_profile = "default".into();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Error;
        support::seed_instances_on_disk_for_test("default", vec![inst.clone()]);
        let state = support::build_test_app_state_with_launcher(vec![inst], launcher);
        support::refresh_canonical_metadata_for_test(&state).await;
        state
    }

    async fn next_change(
        changes: &mut tokio::sync::broadcast::Receiver<crate::server::push::StatusChange>,
    ) -> (crate::session::Status, crate::session::Status) {
        let change = tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv())
            .await
            .expect("a status change")
            .expect("status channel open");
        (change.old, change.new)
    }

    /// #5671: Retry on an Error row shows `Starting`, and a failed launch puts the row back.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_retry_shows_starting_then_returns_to_error() {
        use crate::session::Status::{Error, Starting};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        for endpoint in ["spawn", "restart"] {
            let id = format!("sess-5671-{endpoint}");
            let state = errored_retry_state(project.path(), &id).await;
            let mut changes = state.status_tx.subscribe();
            let response = if endpoint == "spawn" {
                spawn_acp(State(state.clone()), Path(id.clone()), retry_request())
                    .await
                    .into_response()
            } else {
                restart_acp(State(state.clone()), Path(id.clone()), retry_request())
                    .await
                    .into_response()
            };
            assert!(
                !response.status().is_success(),
                "{endpoint}: {}",
                response.status()
            );
            assert_eq!(
                next_change(&mut changes).await,
                (Error, Starting),
                "{endpoint}"
            );
            assert_eq!(
                next_change(&mut changes).await,
                (Starting, Error),
                "{endpoint}"
            );
            assert_eq!(state.instances.read().await[0].status, Error, "{endpoint}");
            assert_eq!(rolled_back(&state, &id), Some(true), "{endpoint}");
        }
    }

    /// #5977: a Retry from a Stopped row shows `Starting` through either handler, and its failed
    /// launch lands in the log as the Agent's startup error: the row ends `Error`, boot's fold
    /// says so, and it notifies once. A dormant `Idle` row's Retry shows no `Starting`.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_retry_from_stopped_shows_starting_then_errors_through_the_handlers() {
        use crate::session::Status::{Error, Idle, Starting, Stopped};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut got = Vec::new();
        for (endpoint, dormant) in [("spawn", false), ("restart", false), ("spawn", true)] {
            let id = format!("sess-5977-{endpoint}-{dormant}");
            let state = errored_retry_state(project.path(), &id).await;
            {
                let mut rows = state.instances.write().await;
                if dormant {
                    rows[0].status = Idle;
                    rows[0].idle_dormant_since = Some(crate::time::Timestamp::now());
                } else {
                    rows[0].status = Stopped;
                }
                crate::server::test_support::seed_instances_on_disk_for_test(
                    "default",
                    rows.clone(),
                );
            }
            let listener =
                tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()));
            let mut changes = state.status_tx.subscribe();
            let _ = if endpoint == "spawn" {
                spawn_acp(State(state.clone()), Path(id.clone()), retry_request())
                    .await
                    .into_response()
            } else {
                restart_acp(State(state.clone()), Path(id.clone()), retry_request())
                    .await
                    .into_response()
            };
            let mut sent = Vec::new();
            if !dormant {
                loop {
                    let change =
                        tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv())
                            .await
                            .expect("a status change")
                            .expect("status channel open");
                    let error = change.new == Error;
                    sent.push(change);
                    if error {
                        break;
                    }
                }
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                state.runtime.work.drain(),
            )
            .await
            .expect("the rollback finishes");
            while let Ok(change) = changes.try_recv() {
                sent.push(change);
            }
            listener.abort();
            let failures: Vec<_> = state
                .acp_event_store
                .replay_from(&id, 0)
                .into_iter()
                .filter_map(|(_, event)| match event {
                    crate::acp::Event::AgentStartupError { message, .. } => Some(message),
                    _ => None,
                })
                .collect();
            got.push((
                endpoint,
                dormant,
                sent.iter().map(|c| (c.old, c.new)).collect::<Vec<_>>(),
                state.instances.read().await[0].status,
                crate::server::acp_events::folded_sidebar_status(&state.session_service, &id).await,
                failures
                    .iter()
                    .map(|m| m.starts_with("Failed to start structured view agent \"codex\""))
                    .collect::<Vec<_>>(),
                crate::server::push::pushes_for_test(sent),
            ));
        }
        let failed = vec![(Stopped, Idle), (Idle, Starting), (Starting, Error)];
        let want = [
            (
                "spawn",
                false,
                failed.clone(),
                Error,
                Some(Error),
                vec![true],
                1,
            ),
            ("restart", false, failed, Error, Some(Error), vec![true], 1),
            ("spawn", true, vec![], Idle, None, vec![], 0),
        ];
        assert_eq!(got, want);
    }

    /// #5977: a Retry from a Stopped row shows `Starting` while its sandbox is prepared, not the
    /// resume's `Idle`. A failed preparation is the Agent's startup error, and a cancelled one
    /// puts the `Idle` back as a rollback, which cues nothing.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_retry_shows_starting_through_sandbox_preparation() {
        use crate::server::acp_events::retry_test_gate::{fail_next_preparation, RevertGate};
        use crate::server::push::StatusChange;
        use crate::session::Status::{Error, Idle, Starting, Stopped};
        async fn recv(
            changes: &mut tokio::sync::broadcast::Receiver<StatusChange>,
            case: &str,
        ) -> StatusChange {
            tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv())
                .await
                .unwrap_or_else(|_| panic!("{case}: a status change"))
                .expect("status channel open")
        }
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let started = vec![(Stopped, Idle), (Idle, Starting)];
        let failed = vec![(Stopped, Idle), (Idle, Starting), (Starting, Error)];
        let cancelled = vec![(Stopped, Idle), (Idle, Starting), (Starting, Idle)];
        // (case, endpoint, what the preparation does, the row's changes and status, at the gate
        // for a parked preparation and once settled otherwise, startup errors, pushes)
        for (case, endpoint, prepare, want_changes, want_status, want_errors, want_pushes) in [
            ("prepare-delayed", "spawn", "park", &started, Starting, 0, 0),
            (
                "prepare-delayed-restart",
                "restart",
                "park",
                &started,
                Starting,
                0,
                0,
            ),
            ("prepare-failed", "spawn", "fail", &failed, Error, 1, 1),
            (
                "prepare-failed-restart",
                "restart",
                "fail",
                &failed,
                Error,
                1,
                1,
            ),
            (
                "prepare-cancelled",
                "spawn",
                "cancel",
                &cancelled,
                Idle,
                0,
                0,
            ),
        ] {
            let id = format!("sess-5977-{case}");
            let state = errored_retry_state(project.path(), &id).await;
            {
                let mut rows = state.instances.write().await;
                rows[0].status = Stopped;
                crate::server::test_support::seed_instances_on_disk_for_test(
                    "default",
                    rows.clone(),
                );
            }
            let listener =
                tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()));
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while state.acp_events_tx.receiver_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener subscribes");
            let mut changes = state.status_tx.subscribe();
            let gate = (prepare != "fail").then(|| RevertGate::arm(&format!("{id}#prepare")));
            if prepare == "fail" {
                fail_next_preparation(&id);
            }
            let (launch_state, launch_id) = (state.clone(), id.clone());
            let mut launch = Box::pin(async move {
                if endpoint == "spawn" {
                    spawn_acp(State(launch_state), Path(launch_id), retry_request())
                        .await
                        .into_response()
                } else {
                    restart_acp(State(launch_state), Path(launch_id), retry_request())
                        .await
                        .into_response()
                }
            });
            let mut sent = Vec::new();
            let status = match gate {
                Some((gate, parked)) => {
                    tokio::select! {
                        response = &mut launch => panic!("{case}: launch ended before its preparation: {}", response.status()),
                        parked = tokio::time::timeout(std::time::Duration::from_secs(10), parked) => {
                            parked.expect("the launch reaches its preparation").expect("gate open");
                        }
                    }
                    // The resume and the Retry's Starting are published before the gate.
                    while let Ok(change) = changes.try_recv() {
                        sent.push(change);
                    }
                    let at_gate = state.instances.read().await[0].status;
                    if prepare == "cancel" {
                        drop(launch);
                        drop(gate);
                        sent.push(recv(&mut changes, case).await);
                        state.instances.read().await[0].status
                    } else {
                        // What the launch does past its preparation is covered elsewhere.
                        drop(gate);
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), launch)
                            .await
                            .expect("the launch finishes");
                        at_gate
                    }
                }
                None => {
                    let _ = launch.await;
                    while sent.last().map(|c: &StatusChange| c.new) != Some(Error) {
                        sent.push(recv(&mut changes, case).await);
                    }
                    state.instances.read().await[0].status
                }
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                state.runtime.work.drain(),
            )
            .await
            .expect("the rollback finishes");
            listener.abort();
            let errors = state
                .acp_event_store
                .replay_from(&id, 0)
                .into_iter()
                .filter(|(_, event)| {
                    matches!(event, crate::acp::Event::AgentStartupError { message, .. }
                        if message.starts_with("Failed to prepare the sandbox"))
                })
                .count();
            assert_eq!(
                (
                    sent.iter().map(|c| (c.old, c.new)).collect::<Vec<_>>(),
                    status,
                    errors,
                    crate::server::push::pushes_for_test(sent),
                ),
                (want_changes.clone(), want_status, want_errors, want_pushes),
                "{case}"
            );
        }
    }

    /// #5921: restart takes its capacity slot before it shows `Starting`. With the only slot held
    /// by another session it is refused with no status change; its own worker in that slot is
    /// stopped first, so it restarts.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_restart_is_admitted_at_capacity_before_it_shows_starting() {
        use crate::acp::supervisor::ResumeKind;
        use crate::process::worker_registry::ResumeAuthority;
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        for held_by_another in [true, false] {
            let id = format!("sess-5921-capacity-{held_by_another}");
            let launcher = crate::server::test_support::succeeding_launcher();
            let state = errored_retry_state_with(project.path(), &id, launcher, None).await;
            let _other = if held_by_another {
                Some(
                    state
                        .acp_supervisor
                        .reserve_spawn("other", ResumeKind::SpawnFirst, ResumeAuthority::Explicit)
                        .await
                        .expect("the other session takes the only slot"),
                )
            } else {
                state.acp_supervisor.test_insert_worker(&id).await;
                None
            };
            let mut changes = state.status_tx.subscribe();
            let response = restart_acp(State(state.clone()), Path(id.clone()), retry_request())
                .await
                .into_response();
            if held_by_another {
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert!(
                    String::from_utf8_lossy(&body).contains("capacity"),
                    "{status}: {}",
                    String::from_utf8_lossy(&body)
                );
                // Every status change is sent before the handler returns.
                assert!(matches!(
                    changes.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ));
                assert_eq!(
                    state.instances.read().await[0].status,
                    crate::session::Status::Error
                );
                assert_eq!(rolled_back(&state, &id), None);
            } else {
                assert!(response.status().is_success(), "{}", response.status());
                assert_ne!(
                    state.acp_supervisor.worker_state(&id).await,
                    crate::daemon::AcpWorkerState::Absent
                );
            }
        }
    }

    /// #5671: a Retry dropped after it wrote `Starting` (the client went away mid-launch) still
    /// puts the row back. Only spawn runs in the request future; restart runs as runtime work.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_dropped_retry_restores_the_prior_status() {
        use crate::session::Status::{Error, Starting};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let [ready, release, done] = ["ready", "release", "done"].map(|f| project.path().join(f));
        let hook = format!(
            ": > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; : > '{}'",
            ready.display(),
            release.display(),
            done.display()
        );
        crate::session::config::update_config(|global| {
            global.host_hooks.before_session = vec![hook];
        })
        .unwrap();
        // Nothing awaits the dropped launch's hook, and the runtime joins its blocking thread at
        // shutdown: release it on every exit path and wait until it has seen the release.
        struct Release([std::path::PathBuf; 2]);
        impl Drop for Release {
            fn drop(&mut self) {
                let [release, done] = &self.0;
                let _ = std::fs::write(release, b"release");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !done.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let release = Release([release, done]);
        let id = "sess-5671-drop".to_string();
        let state = errored_retry_state(project.path(), &id).await;
        let mut changes = state.status_tx.subscribe();
        let mut launch = Box::pin(spawn_acp(State(state.clone()), Path(id), retry_request()));
        let hook_running = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        tokio::select! {
            response = &mut launch => panic!("launch ended before its hook: {}", response.into_response().status()),
            waited = hook_running => waited.expect("before_session hook started"),
        }
        assert_eq!(next_change(&mut changes).await, (Error, Starting));

        drop(launch);
        let restored = next_change(&mut changes).await;
        drop(release);
        assert_eq!(restored, (Starting, Error));
        assert_eq!(state.instances.read().await[0].status, Error);
        assert_eq!(rolled_back(&state, "sess-5671-drop"), Some(true));
    }

    /// #5921: a Retry the agent policy refuses answers 403 before it shows `Starting`, as a
    /// restart does.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_retry_the_agent_policy_refuses_never_shows_starting() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let id = "sess-5921-policy".to_string();
        let state = errored_retry_state(project.path(), &id).await;
        crate::session::config::update_config(|config| {
            config.acp.restrict_agents = true;
            config.acp.allowed_agents = vec!["claude".into()];
        })
        .unwrap();
        let mut changes = state.status_tx.subscribe();
        let response = spawn_acp(State(state.clone()), Path(id.clone()), retry_request())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            state.instances.read().await[0].status,
            crate::session::Status::Error
        );
        assert_eq!(rolled_back(&state, &id), None);
    }

    /// #5921: once a Retry's worker is installed its launch has succeeded, so a request dropped
    /// after that, while its mode is applied or past it, leaves `Starting` for the worker's own
    /// frames rather than rolling back. A worker that then fails still turns the row Error.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_retry_dropped_after_its_worker_is_installed_keeps_starting() {
        use crate::server::acp_events::retry_test_gate::RevertGate;
        use crate::session::Status::{Error, Starting};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        for (gate_at, mode, worker_fails) in [
            ("launched", None, false),
            ("mode", Some("plan"), false),
            ("mode", Some("plan"), true),
        ] {
            let id = format!("sess-5921-{gate_at}-{worker_fails}");
            let (launcher, workers) =
                crate::server::test_support::succeeding_launcher_with_senders();
            let state = errored_retry_state_with(project.path(), &id, launcher, mode).await;
            let listener =
                tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()));
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while state.acp_events_tx.receiver_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener subscribes");
            let (gate, parked) = RevertGate::arm(&format!("{id}#{gate_at}"));
            let mut changes = state.status_tx.subscribe();
            let mut launch = Box::pin(spawn_acp(
                State(state.clone()),
                Path(id.clone()),
                retry_request(),
            ));
            tokio::select! {
                response = &mut launch => panic!("{gate_at}: launch ended before the gate: {}", response.into_response().status()),
                parked = tokio::time::timeout(std::time::Duration::from_secs(10), parked) => {
                    parked.expect("the launch reaches the gate").expect("gate open");
                }
            }
            assert_eq!(next_change(&mut changes).await, (Error, Starting));
            drop(launch);
            drop(gate);
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                state.runtime.work.drain(),
            )
            .await
            .expect("runtime work settles");
            assert!(
                matches!(
                    changes.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "{id}: no rollback"
            );
            assert_eq!(state.instances.read().await[0].status, Starting, "{id}");
            assert_eq!(rolled_back(&state, &id), Some(false), "{id}");
            if worker_fails {
                let worker = workers.lock().unwrap().last().cloned().expect("a worker");
                worker
                    .send(crate::acp::Event::startup_error("adapter exited"))
                    .await
                    .expect("drain open");
                assert_eq!(next_change(&mut changes).await, (Starting, Error), "{id}");
                assert_eq!(
                    rolled_back(&state, &id),
                    None,
                    "{id}: the failure settles it"
                );
            }
            listener.abort();
            let _ = listener.await;
        }
    }

    /// #5921: replacing the worker publishes cancellations and detaches for the retired one,
    /// past the Retry's watermark. Neither the listener, even while the install is still
    /// publishing, nor lag recovery lets them settle `Starting`; the new worker's own frames, a
    /// background one included, still do.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_retry_starting_ignores_what_its_install_publishes_for_the_retired_worker() {
        use crate::acp::Event;
        use crate::server::acp_events::retry_test_gate::RevertGate;
        use crate::session::Status::{Error, Starting};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let task = |id: &str| Event::BackgroundTaskStarted {
            task_id: id.into(),
            tool_call_id: None,
            description: "sleep 60".into(),
            workflow: None,
            started_at: None,
        };
        let approval = Event::ApprovalRequested {
            approval: crate::acp::approvals::Approval {
                nonce: crate::acp::approvals::Nonce("orphan".into()),
                tool_call: crate::acp::state::ToolCall {
                    prompt: None,
                    id: "tc-1".into(),
                    name: "Terminal".into(),
                    kind: "execute".into(),
                    args_preview: "{}".into(),
                    started_at: crate::time::Timestamp::now(),
                    parent_tool_call_id: None,
                    memory_recall: None,
                    diffs: Vec::new(),
                },
                destructive: false,
                options: Vec::new(),
                choice: false,
                plan: None,
                requested_at: crate::time::Timestamp::now(),
                resolved: None,
            },
        };
        // (case, the retired worker's unresolved event, what the new worker sends; None runs
        // lag recovery instead of the listener)
        for (case, orphan, fresh) in [
            (
                "detach",
                Some(task("old")),
                Some(Event::startup_error("adapter exited")),
            ),
            (
                "orphaned-approval",
                Some(approval.clone()),
                Some(Event::startup_error("adapter exited")),
            ),
            (
                "orphaned-approval-mid-install",
                Some(approval),
                Some(Event::startup_error("adapter exited")),
            ),
            ("detach-recovery", Some(task("old")), None),
            ("own-background", None, Some(task("new"))),
        ] {
            let id = format!("sess-5921-retired-{case}");
            let (launcher, workers) =
                crate::server::test_support::succeeding_launcher_with_senders();
            let state = errored_retry_state_with(project.path(), &id, launcher, None).await;
            let mut seq = 0;
            for event in orphan.iter().chain([&Event::Stopped {
                reason: "end_turn".into(),
            }]) {
                seq += 1;
                state.acp_event_store.record(&id, seq, event).unwrap();
            }
            state.acp_supervisor.hydrate_seqs([(id.clone(), seq)]);
            let listener = fresh.is_some().then(|| {
                tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()))
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while listener.is_some() && state.acp_events_tx.receiver_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener subscribes");
            let mut changes = state.status_tx.subscribe();
            let gate = (case == "orphaned-approval-mid-install")
                .then(|| RevertGate::arm(&format!("{id}#retire")));
            let mut launch = Box::pin(spawn_acp(
                State(state.clone()),
                Path(id.clone()),
                retry_request(),
            ));
            let mid_install = gate.is_some();
            if let Some((gate, parked)) = gate {
                tokio::select! {
                    response = &mut launch => panic!("{case}: launch ended before the gate: {}", response.into_response().status()),
                    parked = tokio::time::timeout(std::time::Duration::from_secs(10), parked) => {
                        parked.expect("the install reaches the gate").expect("gate open");
                    }
                }
                assert_eq!(next_change(&mut changes).await, (Error, Starting), "{case}");
                // Queued behind the cleanup frames, so its change proves the listener has
                // handled them while the install had not yet recorded their cutoff.
                let mut sentinel =
                    crate::session::Instance::new("sentinel", project.path().to_str().unwrap());
                sentinel.id = format!("{id}-sentinel");
                sentinel.view = crate::session::View::Structured;
                sentinel.source_profile = "default".into();
                sentinel.status = crate::session::Status::Idle;
                state.instances.write().await.push(sentinel.clone());
                state
                    .acp_events_tx
                    .send(crate::acp::protocol::AcpBroadcastFrame {
                        session_id: sentinel.id.clone(),
                        seq: 1,
                        event: Arc::new(Event::startup_error("sentinel")),
                        worker_generation: None,
                        sidebar_status: None,
                    })
                    .unwrap();
                let change =
                    tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv())
                        .await
                        .expect("a status change")
                        .expect("status channel open");
                assert_eq!(
                    (change.instance_id.as_str(), change.new),
                    (sentinel.id.as_str(), Error),
                    "{case}: the retired worker's frames moved the row first"
                );
                assert_eq!(state.instances.read().await[0].status, Starting, "{case}");
                assert_eq!(rolled_back(&state, &id), Some(false), "{case}");
                gate.release();
            }
            let response = launch.await.into_response();
            assert!(
                response.status().is_success(),
                "{case}: {}",
                response.status()
            );
            if !mid_install {
                assert_eq!(next_change(&mut changes).await, (Error, Starting), "{case}");
            }
            assert_eq!(
                state.acp_supervisor.latest_seq(&id) > seq,
                orphan.is_some(),
                "{case}: the install published for the retired worker"
            );
            match fresh {
                Some(event) => {
                    let worker = workers.lock().unwrap().last().cloned().expect("a worker");
                    worker.send(event).await.expect("drain open");
                    let change = next_change(&mut changes).await;
                    if case == "own-background" {
                        assert_eq!(change.0, Starting, "{case}: {change:?}");
                        assert_ne!(change.1, Starting, "{case}");
                    } else {
                        assert_eq!(change, (Starting, Error), "{case}");
                    }
                    assert_eq!(rolled_back(&state, &id), None, "{case}: settled");
                }
                None => {
                    crate::server::acp_events::recover_structured_unread_after_lag(
                        &state.instances,
                        &state.session_service,
                        &state.instance_locks,
                        crate::file_watch::FileWatchService::noop(),
                        &state.status_tx,
                        &state.retry_starting,
                    )
                    .await;
                    assert!(
                        changes.try_recv().is_err(),
                        "{case}: recovery left Starting"
                    );
                    assert_eq!(state.instances.read().await[0].status, Starting, "{case}");
                    assert_eq!(rolled_back(&state, &id), Some(false), "{case}");
                }
            }
            if let Some(listener) = listener {
                listener.abort();
                let _ = listener.await;
            }
        }
    }

    /// #5921: a status read that scans while an install holds its provisional cutoff and applies
    /// after the final one is written leaves the Retry's Starting, and its owner, alone: the scan
    /// found no status evidence, which is not a seq newer than the cutoff.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_scan_inside_the_retire_fence_leaves_starting() {
        use crate::acp::Event;
        use crate::server::acp_events::retry_test_gate::RevertGate;
        use crate::session::Status::{Error, Starting};
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        // (case, the status read, whether the attempt is rolled back, the retired worker's last
        // event)
        for (case, seed, rolled, last) in [
            (
                "lag",
                false,
                false,
                Event::Stopped {
                    reason: "end_turn".into(),
                },
            ),
            (
                "seed",
                true,
                false,
                Event::Stopped {
                    reason: "end_turn".into(),
                },
            ),
            (
                "rolled-back",
                false,
                true,
                Event::startup_error("adapter exited"),
            ),
        ] {
            let id = format!("sess-5921-fence-{case}");
            let launcher = crate::server::test_support::succeeding_launcher();
            let state = errored_retry_state_with(project.path(), &id, launcher, None).await;
            state.acp_event_store.record(&id, 1, &last).unwrap();
            state.acp_supervisor.hydrate_seqs([(id.clone(), 1)]);
            let mut changes = state.status_tx.subscribe();
            let (install, installing) = RevertGate::arm(&format!("{id}#retire"));
            let mut launch = Box::pin(spawn_acp(
                State(state.clone()),
                Path(id.clone()),
                retry_request(),
            ));
            tokio::select! {
                response = &mut launch => panic!("{case}: launch ended before the gate: {}", response.into_response().status()),
                parked = tokio::time::timeout(std::time::Duration::from_secs(10), installing) => {
                    parked.expect("the install reaches the gate").expect("gate open");
                }
            }
            assert_eq!(next_change(&mut changes).await, (Error, Starting), "{case}");
            let (epoch, fence) = state.acp_supervisor.retired_through(&id).expect("a fence");
            assert_eq!(fence, u64::MAX, "{case}: the provisional cutoff");
            if rolled {
                crate::server::acp_events::RetryAttempt::mark_rolled_back_for_test(
                    &state.retry_starting,
                    &id,
                );
            }

            let (read, scanned) = RevertGate::arm(&format!("{id}#evidence"));
            let status_read = tokio::spawn({
                let state = state.clone();
                let id = id.clone();
                async move {
                    if seed {
                        crate::server::acp_reconciler::seed_in_flight_status(&state, &id).await;
                    } else {
                        crate::server::acp_events::recover_structured_unread_after_lag(
                            &state.instances,
                            &state.session_service,
                            &state.instance_locks,
                            crate::file_watch::FileWatchService::noop(),
                            &state.status_tx,
                            &state.retry_starting,
                        )
                        .await;
                    }
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), scanned)
                .await
                .expect("the status read scans")
                .expect("gate open");
            install.release();
            let response = launch.await.into_response();
            assert!(
                response.status().is_success(),
                "{case}: {}",
                response.status()
            );
            let (now_epoch, cutoff) = state.acp_supervisor.retired_through(&id).expect("a cutoff");
            assert!(
                now_epoch == epoch && cutoff != u64::MAX,
                "{case}: the final cutoff is written: {cutoff}"
            );
            read.release();
            status_read.await.unwrap();

            // Every change the status read makes is sent before it returns.
            assert!(
                changes.try_recv().is_err(),
                "{case}: the status read moved the row"
            );
            assert_eq!(state.instances.read().await[0].status, Starting, "{case}");
            assert_eq!(
                rolled_back(&state, &id),
                Some(rolled),
                "{case}: the owner survives"
            );
        }
    }

    /// #4116: a peer archive committed while the `before_session` hook runs (after the handler's
    /// recheck) still refuses the launch, on both the spawn and the agent-switch endpoint.
    #[tokio::test]
    #[serial_test::serial]
    async fn launch_refuses_a_row_archived_while_the_before_session_hook_runs() {
        use crate::server::test_support as support;
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let barrier = tempfile::tempdir().unwrap();
        for endpoint in ["spawn", "switch"] {
            let hook = support::install_blocking_before_session_hook(barrier.path(), endpoint);
            let mut inst =
                crate::session::Instance::new("acp-4116-hook", barrier.path().to_str().unwrap());
            inst.id = format!("sess-4116-hook-{endpoint}");
            inst.source_profile = "default".into();
            inst.view = crate::session::View::Structured;
            inst.status = crate::session::Status::Idle;
            let id = inst.id.clone();
            let profile = inst.source_profile.clone();
            support::seed_instances_on_disk_for_test(&profile, vec![inst.clone()]);
            let (launcher, launches) = support::counting_failing_launcher();
            let state = support::build_test_app_state_with_launcher(vec![inst], launcher);

            let handler = tokio::spawn({
                let state = Arc::clone(&state);
                let id = id.clone();
                async move {
                    if endpoint == "spawn" {
                        spawn_acp(
                            State(state),
                            Path(id),
                            Ok(Json(worker::SpawnAcpRequest {
                                agent: None,
                                model: None,
                                additional_dirs: Vec::new(),
                                provider_env: Vec::new(),
                            })),
                        )
                        .await
                        .into_response()
                    } else {
                        switch_acp_agent(
                            State(state),
                            Path(id),
                            Ok(Json(crate::acp::protocol::SwitchAgentRequest {
                                target: "codex".to_string(),
                                model: None,
                                reason: None,
                            })),
                        )
                        .await
                        .into_response()
                    }
                }
            });
            let archived =
                support::archive_while_hook_waits(&hook, &profile, |row| row.id == id).await;
            let response = handler.await.unwrap();

            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(
                archived,
                "{endpoint}: before_session hook did not run: {status} {body}"
            );
            assert_eq!(status, StatusCode::CONFLICT, "{endpoint}: {body}");
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"], "session_archived", "{endpoint}");
            assert_eq!(
                launches.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{endpoint}: launched"
            );
            assert!(!state.acp_supervisor.is_running(&id).await, "{endpoint}");
        }
    }

    /// #3650: endpoints that tear the worker down wait for an in-flight
    /// submission, or `send_turn` would respawn the worker they stop.
    #[tokio::test]
    #[serial_test::serial]
    async fn worker_stopping_acp_endpoints_wait_for_an_in_flight_submission() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        use std::time::Duration;

        for which in ["shutdown", "switch", "disable"] {
            let mut inst =
                crate::session::Instance::new("acp-3650", project.path().to_str().unwrap());
            inst.id = format!("sess-3650-acp-{which}");
            inst.source_profile = "default".into();
            inst.view = crate::session::View::Structured;
            inst.status = crate::session::Status::Idle;
            inst.acp_load_session_capable = Some(true);
            inst.command = "sleep 60".into();
            let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
                crate::tmux::Session::generate_name(&inst.id, &inst.title),
            );
            let id = inst.id.clone();
            if which == "disable" {
                crate::session::Storage::new_unwatched(&inst.source_profile)
                    .unwrap()
                    .update(|rows, _| {
                        rows.push(inst.clone());
                        Ok(())
                    })
                    .unwrap();
            } else {
                crate::server::test_support::seed_instances_on_disk_for_test(
                    &inst.source_profile,
                    vec![inst.clone()],
                );
            }
            // A real switch launch dials the host's `codex-acp`, if any, for up to 10s.
            let (launcher, _) = crate::server::test_support::counting_failing_launcher();
            let state = crate::server::test_support::build_test_app_state_with_launcher(
                vec![inst],
                launcher,
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;

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
                            Ok(Json(crate::acp::protocol::SwitchAgentRequest {
                                target: "codex".to_string(),
                                model: None,
                                reason: None,
                            })),
                        )
                        .await
                        .into_response(),
                        _ => acp_disable(state, id, Default::default())
                            .await
                            .into_response(),
                    }
                }
            };
            tokio::pin!(handler);
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! {
                    claim = claims.recv() => {
                        assert_eq!(claim.expect("contender reached claim"), id, "{which}");
                    }
                    response = &mut handler => panic!(
                        "{which} answered before claiming submission exclusion: {}",
                        response.status(),
                    ),
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{which} must reach the held submission claim"));
            assert!(futures_util::poll!(&mut handler).is_pending(), "{which}");

            drop(delivering);
            let response = tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"));
            if which == "disable" {
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "disable: {}",
                    String::from_utf8_lossy(&body)
                );
                let instances = state.instances.read().await;
                let inst = instances.iter().find(|inst| inst.id == id).unwrap();
                assert_eq!(inst.acp_load_session_capable, None);
            }
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_handoff_rejects_an_acp_identity_changed_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&temp.path().join("app"));
        let _path = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 1\n",
        );
        let _home = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path().to_path_buf()),
            ("CLAUDE_CONFIG_DIR", temp.path().join(".claude")),
        ]);
        let profile = "handoff-cas";
        let sid = "11111111-1111-4111-8111-111111111111";
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        for changed in [Some("22222222-2222-4222-8222-222222222222"), None] {
            let mut inst = crate::session::Instance::new("handoff", temp.path().to_str().unwrap());
            inst.source_profile = profile.into();
            inst.tool = "claude".into();
            inst.command = "claude".into();
            inst.agent_name = Some("claude-agent-acp".into());
            inst.resume_binding = Some(inst.asserted_resume_binding(sid, None).unwrap());
            inst.resume_intent = crate::session::ResumeIntent::Use(sid.into());
            inst.view = crate::session::View::Structured;
            inst.acp_session_id = Some(sid.into());
            let id = inst.id.clone();
            let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
            inst.acp_session_id = changed.map(str::to_owned);
            storage
                .update(|rows, _| {
                    *rows = vec![inst.clone()];
                    Ok(())
                })
                .unwrap();

            let response = acp_disable(State(state.clone()), Path(id.clone()), Default::default())
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let rows = storage.load().unwrap();
            let row = rows.iter().find(|row| row.id == id).unwrap();
            assert_eq!(row.view, crate::session::View::Structured);
            assert_eq!(row.acp_session_id.as_deref(), changed);
            assert_eq!(
                state.instances.read().await[0].view,
                crate::session::View::Structured
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn terminal_handoff_cas_follows_the_adopted_durable_row_through_a_reload() {
        use std::{future::Future, task::Poll, time::Duration};

        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                for (reload, expected_status) in [
                    (None, StatusCode::OK),
                    (Some(false), StatusCode::CONFLICT),
                    (Some(true), StatusCode::OK),
                ] {
                    let profile = "handoff-reload";
                    let project = tempfile::tempdir().unwrap();
                    let mut cached =
                        crate::session::Instance::new("handoff", project.path().to_str().unwrap());
                    cached.tool = "shell".into();
                    cached.command = "sleep 60".into();
                    cached.source_profile = profile.into();
                    let id = cached.id.clone();
                    let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
                        crate::tmux::Session::generate_name(&id, &cached.title),
                    );
                    let mut durable = cached.clone();
                    durable.view = crate::session::View::Structured;
                    durable.acp_session_id = Some("11111111-1111-4111-8111-111111111111".into());
                    let storage = crate::session::Storage::new_unwatched(profile).unwrap();
                    storage
                        .update(|all, _| {
                            *all = vec![durable.clone()];
                            Ok(())
                        })
                        .unwrap();
                    let state = terminal_handoff_test_state(vec![cached], temp.path());
                    crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let holder = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    });
                    entered_rx.await.unwrap();
                    let lock = state.instance_lock(&id).await;
                    let mut handler = Box::pin(acp_disable(
                        State(state.clone()),
                        Path(id.clone()),
                        Default::default(),
                    ));
                    std::future::poll_fn(|cx| {
                        assert!(handler.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while lock.try_lock().is_ok() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("tracked handoff task must acquire its instance exclusion");
                    assert!(lock.try_lock().is_err());
                    if let Some(same_identity) = reload {
                        let mut fresh = durable;
                        if !same_identity {
                            fresh.acp_session_id =
                                Some("22222222-2222-4222-8222-222222222222".into());
                        }
                        let metadata = state.canonical_metadata.read().await.clone();
                        crate::server::reload::reload_state_instances_from_disk(
                            &state,
                            vec![fresh].into(),
                            Vec::new(),
                            crate::server::state::StatusSource::DiskOnly,
                            state
                                .mutation_epoch
                                .load(std::sync::atomic::Ordering::SeqCst),
                            Ok(metadata),
                            Default::default(),
                        )
                        .await;
                    }
                    release_tx.send(()).unwrap();
                    holder.await.unwrap();
                    let response = tokio::time::timeout(Duration::from_secs(10), handler)
                        .await
                        .unwrap()
                        .into_response();
                    let status = response.status();
                    let body = axum::body::to_bytes(response.into_body(), 4096)
                        .await
                        .unwrap();
                    assert_eq!(
                        status,
                        expected_status,
                        "reload={reload:?}: {}",
                        String::from_utf8_lossy(&body)
                    );
                    let expected_sid = if expected_status == StatusCode::OK {
                        None
                    } else {
                        Some("22222222-2222-4222-8222-222222222222".to_string())
                    };
                    let expected_view = if expected_status == StatusCode::OK {
                        crate::session::View::Terminal
                    } else {
                        crate::session::View::Structured
                    };
                    let cached = state.instances.read().await;
                    assert_eq!(cached[0].view, expected_view);
                    assert_eq!(cached[0].acp_session_id, expected_sid);
                    let durable_rows = storage.load().unwrap();
                    assert_eq!(durable_rows[0].view, expected_view);
                    let durable_sid = if expected_status == StatusCode::OK {
                        None
                    } else {
                        Some("11111111-1111-4111-8111-111111111111".to_string())
                    };
                    assert_eq!(durable_rows[0].acp_session_id, durable_sid);
                }
            });
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_handoff_does_not_lock_other_sessions_during_storage_contention() {
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "handoff-contention";
        let project = tempfile::tempdir().unwrap();
        let mut inst = crate::session::Instance::new("handoff", project.path().to_str().unwrap());
        inst.tool = "shell".into();
        inst.command = "sleep 60".into();
        inst.source_profile = profile.into();
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("original-acp".into());
        inst.agent_session_id = Some("native-preserved".into());
        let id = inst.id.clone();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&id, &inst.title),
        );
        let expected = inst.conversation_state();
        let mut other = crate::session::Instance::new("other", "/tmp/other");
        other.source_profile = profile.into();
        other.view = crate::session::View::Structured;
        let other_id = other.id.clone();
        let rows = vec![inst, other];
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|all, _| {
                *all = rows.clone();
                Ok(())
            })
            .unwrap();
        let state = terminal_handoff_test_state(rows, temp.path());
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let listener = tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()));
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.acp_events_tx.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = tokio::task::spawn_blocking(move || {
            storage
                .update(|_, _| {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv_timeout(Duration::from_secs(10));
                    Ok(())
                })
                .unwrap();
        });
        entered_rx.await.unwrap();
        let lock = state.instance_lock(&id).await;
        let handler = tokio::spawn({
            let state = state.clone();
            let id = id.clone();
            async move {
                acp_disable(State(state), Path(id), Default::default())
                    .await
                    .into_response()
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while lock.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        for (seq, event) in [
            crate::acp::Event::AcpSessionAssigned {
                acp_session_id: "late".into(),
            },
            crate::acp::Event::SessionContextReset {
                reason: "late".into(),
            },
            crate::acp::Event::SessionCleared,
        ]
        .into_iter()
        .enumerate()
        {
            state
                .acp_events_tx
                .send(crate::server::AcpBroadcastFrame {
                    session_id: id.clone(),
                    seq: seq as u64 + 1,
                    event: Arc::new(event),
                    worker_generation: None,
                    sidebar_status: None,
                })
                .unwrap();
        }
        let accessible = tokio::time::timeout(Duration::from_secs(2), async {
            let rows = state.instances.read().await;
            assert_eq!(
                rows.iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .acp_session_id
                    .as_deref(),
                Some("original-acp")
            );
            drop(rows);
            let mut rows = state.instances.write().await;
            rows.iter_mut()
                .find(|row| row.id == other_id)
                .unwrap()
                .title = "edited during contention".into();
        })
        .await;
        let still_waiting = !handler.is_finished();
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(10), handler)
            .await
            .unwrap()
            .unwrap();
        assert!(
            accessible.is_ok(),
            "storage contention blocked the global instance cache"
        );
        assert!(still_waiting);
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        state
            .acp_events_tx
            .send(crate::server::AcpBroadcastFrame {
                session_id: other_id.clone(),
                seq: 1,
                event: Arc::new(crate::acp::Event::AcpSessionAssigned {
                    acp_session_id: "drained".into(),
                }),
                worker_generation: None,
                sidebar_status: None,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == other_id)
                    .unwrap()
                    .acp_session_id
                    .as_deref()
                    == Some("drained")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        listener.abort();
        let rows = state.instances.read().await;
        let row = rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.view, crate::session::View::Terminal);
        assert_eq!(row.acp_session_id, None);
        assert!(expected.matches(row));
        drop(rows);
        let rows = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        let row = rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.view, crate::session::View::Terminal);
        assert_eq!(row.acp_session_id, None);
        assert!(expected.matches(row));
    }
}

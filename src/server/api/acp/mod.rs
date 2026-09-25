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
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Persisted,
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
        claude_store_pin: instance
            .selected_claude_conversation()
            .and_then(|(_, execution)| crate::session::capture::ClaudeStorePin::of(execution)),
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
            if which == "disable" {
                crate::session::Storage::new_unwatched(&inst.source_profile)
                    .unwrap()
                    .update(|rows, _| {
                        rows.push(inst.clone());
                        Ok(())
                    })
                    .unwrap();
            }
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

            let response = acp_disable(State(state.clone()), Path(id.clone()))
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
                    let mut cached = crate::session::Instance::new(
                        "handoff",
                        temp.path().join("missing").to_str().unwrap(),
                    );
                    cached.tool = "shell".into();
                    cached.source_profile = profile.into();
                    let id = cached.id.clone();
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
                    let state = crate::server::test_support::build_test_app_state(vec![cached]);
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let holder = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    });
                    entered_rx.await.unwrap();
                    let lock = state.instance_lock(&id).await;
                    let mut handler = Box::pin(acp_disable(State(state.clone()), Path(id.clone())));
                    std::future::poll_fn(|cx| {
                        assert!(handler.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    assert!(lock.try_lock().is_err());
                    if let Some(same_identity) = reload {
                        let mut fresh = durable;
                        if !same_identity {
                            fresh.acp_session_id =
                                Some("22222222-2222-4222-8222-222222222222".into());
                        }
                        crate::server::reload::reload_state_instances_from_disk(
                            &state,
                            vec![fresh],
                            Vec::new(),
                            crate::server::state::StatusSource::DiskOnly,
                            state
                                .mutation_epoch
                                .load(std::sync::atomic::Ordering::SeqCst),
                        )
                        .await;
                    }
                    release_tx.send(()).unwrap();
                    holder.await.unwrap();
                    let response = tokio::time::timeout(Duration::from_secs(10), handler)
                        .await
                        .unwrap()
                        .into_response();
                    assert_eq!(response.status(), expected_status, "reload={reload:?}");
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
        let mut inst =
            crate::session::Instance::new("handoff", temp.path().join("missing").to_str().unwrap());
        inst.tool = "shell".into();
        inst.source_profile = profile.into();
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("original-acp".into());
        inst.agent_session_id = Some("native-preserved".into());
        let id = inst.id.clone();
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
        let state = crate::server::test_support::build_test_app_state(rows);
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
            async move { acp_disable(State(state), Path(id)).await.into_response() }
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
        assert_eq!(response.status(), StatusCode::OK);
        state
            .acp_events_tx
            .send(crate::server::AcpBroadcastFrame {
                session_id: other_id.clone(),
                seq: 1,
                event: Arc::new(crate::acp::Event::AcpSessionAssigned {
                    acp_session_id: "drained".into(),
                }),
                worker_generation: None,
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

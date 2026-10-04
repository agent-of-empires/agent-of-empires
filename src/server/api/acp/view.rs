//! Switching a session between the terminal (tmux) and structured views.

use serde::Serialize;

use crate::server::api::{find_instance, instance_exists};
use crate::session::{Instance, ResumeIntent, Status, View};

use super::*;

#[derive(Debug, Serialize)]
pub struct ViewSwitchResponse {
    pub session_id: String,
    pub view: View,
}

type HandoffSnapshot = (View, Option<String>, crate::session::ConversationState);

fn view_response(session_id: String, view: View) -> Response {
    Json(ViewSwitchResponse { session_id, view }).into_response()
}

fn internal_error(message: &'static str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
}

async fn reserve_view_stop(
    state: &AppState,
    instance: &Instance,
) -> Result<Arc<crate::session::runner_journal::OwnedStop>, Response> {
    let original = state
        .capture_operation_origin(instance)
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()).into_response())?;
    tokio::task::spawn_blocking(move || {
        crate::session::runner_journal::reserve_stop_from_origin(original, false)
    })
    .await
    .map_err(|_| internal_error("Failed to claim original view Stop"))?
    .map_err(|error| (StatusCode::CONFLICT, error.to_string()).into_response())
}

/// How a structured-view spawn seeds its transcript when the view is enabled.
struct StructuredSeed {
    stored_acp_session_id: Option<String>,
    seed_history_replay: bool,
    import_terminal: bool,
}

/// An existing `acp_session_id` is loaded (replayed only when importing).
/// Otherwise a CLI-resumable terminal transcript in `agent_session_id` is
/// carried into a seeded `session/load` (#2252), if the transcript is present.
fn resolve_structured_seed(
    tool: &str,
    acp_agent: &str,
    acp_session_id: Option<&str>,
    agent_session_id: Option<&str>,
    import_pending: bool,
    transcript_present: bool,
) -> StructuredSeed {
    if let Some(id) = acp_session_id {
        return StructuredSeed {
            stored_acp_session_id: Some(id.to_string()),
            seed_history_replay: import_pending,
            import_terminal: false,
        };
    }
    if crate::agents::acp_transcript_cli_resumable(tool, acp_agent) && transcript_present {
        if let Some(id) = agent_session_id.filter(|s| !s.trim().is_empty()) {
            return StructuredSeed {
                stored_acp_session_id: Some(id.to_string()),
                seed_history_replay: true,
                import_terminal: true,
            };
        }
    }
    StructuredSeed {
        stored_acp_session_id: None,
        seed_history_replay: import_pending,
        import_terminal: false,
    }
}

fn adopt_persisted_structured_instance(
    mut cached: Instance,
    mut persisted: Instance,
    source_profile: &str,
) -> Instance {
    persisted.source_profile = source_profile.to_owned();
    crate::server::reload::merge_runtime_fields(&mut cached, persisted)
}

pub async fn acp_enable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match tokio::spawn(acp_enable_owned(State(state), Path(id))).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("view owner task failed: {error}"),
        )
            .into_response(),
    }
}

async fn acp_enable_owned(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    if !instance_exists(&state, &id).await {
        return session_not_found();
    }
    // Serializes the transition with disable and with the deferred spawn.
    let inst_lock = state.instance_lock(&id).await;
    let transition_guard = inst_lock.lock().await;
    let Some(mut instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    let selected_conversation = instance
        .selected_claude_conversation()
        .map(|(sid, execution)| (sid.to_owned(), execution.clone()));
    let selected_binding = selected_conversation.as_ref().and_then(|_| {
        instance
            .conversation_target()
            .and_then(|(_, binding, _)| binding.cloned())
    });
    if instance.is_structured() {
        return view_response(id, View::Structured);
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    let stop = match reserve_view_stop(&state, &instance).await {
        Ok(stop) => stop,
        Err(response) => return response,
    };

    // Judged on the explicit agent (or the tool), not `pick_agent_for_tool`'s
    // default fallback, which would accept every tool.
    let resolvable = {
        let profile = instance.source_profile.clone();
        let project_path = PathBuf::from(&instance.project_path);
        let tool = instance.tool.clone();
        let agent_name = instance.agent_name.clone();
        tokio::task::spawn_blocking(move || {
            super::super::sessions::agent_is_acp_capable(
                &profile,
                &project_path,
                &tool,
                agent_name.as_deref(),
            )
        })
        .await
        .unwrap_or(false)
    };
    if !resolvable {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "no structured view agent registered for tool {:?}",
                instance.tool
            ),
        )
            .into_response();
    }

    let agent_name = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
    if let Err(error) = state
        .acp_supervisor
        .shutdown_and_require_dead(stop.clone())
        .await
    {
        return (
            StatusCode::CONFLICT,
            format!("Cannot switch execution views before stored executions are stopped: {error}"),
        )
            .into_response();
    }
    if let Err(resp) =
        commit_structured_view(&state, &mut instance, selected_binding.as_ref(), stop).await
    {
        return resp;
    }
    spawn_enabled_worker(
        state.clone(),
        inst_lock.clone(),
        instance,
        agent_name,
        selected_conversation,
    );
    drop(transition_guard);
    view_response(id, View::Structured)
}

/// Kill the tmux side and persist the structured view, then mirror it into
/// memory unless a newer lifecycle generation already landed.
async fn commit_structured_view(
    state: &AppState,
    instance: &mut Instance,
    selected_binding: Option<&crate::session::ConversationBinding>,
    stop: Arc<crate::session::runner_journal::OwnedStop>,
) -> Result<(), Response> {
    let original = stop.original_arc();
    let claimed = stop.cancellation_origin();
    let binding_for_transition = selected_binding.cloned();
    let lifecycle_generation = stop.generation();
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        crate::session::runner_journal::finish_owned_stop(&stop, |current| {
            let storage = stop.storage();
            let mut pane = current.clone();
            pane.storage_origin = Some(Arc::new(storage.clone()));
            if let Err(error) = pane.kill_locked() {
                tracing::warn!(target: "acp.switch", session = %pane.id, %error, "kill tmux failed");
            }
            pane.kill_ancillary_tmux_sessions_locked();
            storage.update_native_under_workspace_claim_lock(|all, _| {
                let slot = all.iter_mut().find(|candidate| candidate.id == pane.id).ok_or_else(|| anyhow::anyhow!("original session disappeared during terminal-to-ACP transition"))?;
                slot.view = View::Structured;
                slot.resume_intent = ResumeIntent::Default;
                if let Some(binding) = &binding_for_transition {
                    slot.agent_session_id = Some(binding.session_id.clone());
                    slot.agent_session_binding = Some(binding.clone());
                }
                slot.status = Status::Idle;
                Ok(lifecycle_generation)
            })
        })
    })
    .await;
    let id = instance.id.clone();
    let lifecycle_generation = match transition {
        Ok(Ok(generation)) => generation,
        Ok(Err(error)) => {
            tracing::error!(target: "acp.switch", session = %id, "terminal-to-ACP transition failed: {error:#}");
            return Err(internal_error(
                "failed to switch session to structured view",
            ));
        }
        Err(join_error) => {
            tracing::error!(target: "acp.switch", session = %id, "terminal-to-ACP transition task panicked: {join_error}");
            return Err(internal_error(
                "failed to switch session to structured view",
            ));
        }
    };
    let apply = |inst: &mut Instance| {
        inst.view = View::Structured;
        inst.resume_intent = ResumeIntent::Default;
        if let Some(binding) = selected_binding {
            inst.agent_session_id = Some(binding.session_id.clone());
            inst.agent_session_binding = Some(binding.clone());
        }
        inst.status = Status::Idle;
        inst.lifecycle_generation = lifecycle_generation;
        inst.acp_load_session_capable = None;
    };
    apply(instance);
    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|candidate| {
        original.matches_instance(candidate) || claimed.matches_instance(candidate)
    }) {
        {
            apply(slot);
            state
                .mutation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Spawn the worker in the background so the response does not wait on a
/// container pull. Failures surface as a startup-error banner.
fn spawn_enabled_worker(
    state: Arc<AppState>,
    inst_lock: Arc<tokio::sync::Mutex<()>>,
    instance: Instance,
    agent_name: String,
    selected_conversation: Option<(String, crate::session::ExecutionBinding)>,
) {
    let original = match crate::session::LaunchOrigin::capture(&instance) {
        Ok(original) => original,
        Err(error) => {
            state
                .acp_supervisor
                .publish_startup_error(&instance.id, error.to_string());
            return;
        }
    };
    let admission = state.acp_supervisor.begin_resume(
        &instance.id,
        crate::acp::runner_lifecycle::NativeResume::Spawn,
        original.clone(),
        true,
    );
    let claude_store_pin = instance.selected_claude_store_pin();
    let resume_sid = selected_conversation
        .as_ref()
        .map(|(sid, _)| sid.as_str())
        .or(instance.agent_session_id.as_deref());
    let transcript_present = instance.is_sandboxed()
        || resume_sid
            .map(|sid| {
                !crate::session::capture::claude_host_transcript_confirmed_absent(
                    &instance.project_path,
                    sid,
                    &instance.resolved_host_environment(),
                    claude_store_pin
                        .as_ref()
                        .map(|pin| pin.store.as_path())
                        .or(instance
                            .declared_agent_config_dir_for(&instance.tool)
                            .as_deref()),
                )
            })
            .unwrap_or(false);
    let seed = resolve_structured_seed(
        &instance.tool,
        &agent_name,
        instance.acp_session_id.as_deref(),
        resume_sid,
        instance.import_pending == Some(true),
        transcript_present,
    );
    tokio::spawn(async move {
        // Held through the spawn so a following disable cannot tear down
        // first and then be undone by this late task.
        let _transition_guard = inst_lock.lock().await;
        let session_id = instance.id.clone();
        let still_structured = state
            .instances
            .read()
            .await
            .iter()
            .any(|candidate| original.matches_instance(candidate) && candidate.is_structured());
        if !still_structured {
            return;
        }
        let supervisor = &state.acp_supervisor;
        let reservation = match admission.await {
            Ok(crate::acp::supervisor::ResumeReservationOutcome::Reserved(reservation)) => {
                reservation
            }
            Ok(crate::acp::supervisor::ResumeReservationOutcome::AlreadyPresent) => return,
            Err(error) => {
                supervisor.publish_startup_error(&session_id, error.to_string());
                return;
            }
        };
        let issuance = reservation.execution_admission();
        let _body_custody = issuance.begin_job();
        let sandbox_info = match crate::acp::sandbox::ensure_container_for_session_locked(
            &state.instances,
            issuance.clone(),
            false,
        )
        .await
        {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(target: "acp.switch", session = %session_id, "container ensure failed: {e}");
                supervisor
                    .publish_startup_error(&session_id, format!("container start failed: {e}"));
                return;
            }
        };
        let request = SpawnRequest {
            stored_acp_session_id: seed.stored_acp_session_id,
            seed_history_replay: seed.seed_history_replay,
            sandbox_continuation: if seed.import_terminal {
                crate::acp::supervisor::SandboxContinuation::ImportTerminal
            } else {
                crate::acp::supervisor::SandboxContinuation::Persisted
            },
            claude_store_pin,
            origin: issuance.origin(),
            ..spawn_request_for(
                &instance,
                agent_name.clone(),
                sandbox_info,
                issuance
                    .origin()
                    .expect("prepared view admission owns its original"),
            )
        };
        if let Err(e) = supervisor.spawn_inner(request, reservation).await {
            let message = structured_spawn_error_message(&e, &agent_name);
            tracing::warn!(target: "acp.switch", session = %session_id, "spawn after enable: {message}");
            supervisor.publish_startup_error(&session_id, message);
        }
    });
}

/// Switch a structured session back to tmux. When the agent shares a
/// CLI-resumable transcript and an ACP session id exists, the conversation is
/// kept and tmux resumes it (#2252); otherwise the transcript is deleted.
pub async fn acp_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match tokio::spawn(acp_disable_owned(State(state), Path(id))).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("view owner task failed: {error}"),
        )
            .into_response(),
    }
}

async fn acp_disable_owned(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    // Worker-stopping barrier (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;
    let Some(mut instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    let stop = match reserve_view_stop(&state, &instance).await {
        Ok(stop) => stop,
        Err(response) => return response,
    };
    let before_terminal = stop.current_projection();
    let profile = instance.source_profile.clone();
    let memory_expected = (
        instance.view,
        instance.acp_session_id.clone(),
        instance.conversation_state(),
    );

    if !instance.is_structured() {
        // A reload may have cached a pre-enable terminal snapshot; trust the
        // durable row before answering idempotently.
        let scoped = stop.clone();
        let persisted =
            tokio::task::spawn_blocking(move || scoped.with_scope(|row| Ok(Some(row.clone()))))
                .await
                .unwrap_or_else(|error| Err(anyhow::anyhow!(error)));
        match persisted {
            Ok(Some(durable)) if durable.is_structured() => {
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
            Ok(Some(_)) => {
                // Even an idempotent terminal response must prove stored history:
                // no daemon handle or ACP projection does not mean no execution.
                let cleanup_projection = state.acp_supervisor.is_owned(&id).await
                    || state.acp_event_store.highest_seq(&id) > 0;
                let retry = if cleanup_projection {
                    state.acp_supervisor.shutdown_and_delete(stop.clone()).await
                } else {
                    state
                        .acp_supervisor
                        .shutdown_and_require_dead(stop.clone())
                        .await
                };
                match retry {
                    Ok(()) => {}
                    Err(SupervisorError::TeardownPending(_)) => {
                        return (
                            StatusCode::CONFLICT,
                            format!(
                            "Session {id} is not proven stopped; retry once its executions exit"
                        ),
                        )
                            .into_response()
                    }
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Session {id} cleanup failed: {error}"),
                        )
                            .into_response()
                    }
                }
                if cleanup_projection {
                    return finish_terminal_switch(&state, &id, instance, stop, before_terminal)
                        .await;
                }
                return view_response(id, View::Terminal);
            }
            Ok(None) => return session_not_found(),
            Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
        }
    }

    let disk_expected = (
        instance.view,
        instance.acp_session_id.clone(),
        instance.conversation_state(),
    );

    // Resolve the active adapter: switch_acp_agent can point away from the default.
    let acp_agent = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
    let keep_context = crate::agents::acp_transcript_cli_resumable(&instance.tool, &acp_agent)
        && instance.acp_session_id.is_some();
    if keep_context {
        tracing::debug!(
            target: "acp.switch",
            session = %id,
            "keeping context on disable: carrying acp_session_id into agent_session_id for claude --resume"
        );
        let worker = state
            .acp_supervisor
            .native_handoff_store(
                &id,
                instance.acp_session_id.as_deref().expect("keep-context ID"),
            )
            .await;
        if let Err(error) = instance.switch_to_terminal_keep_context(worker.as_ref()) {
            // The dashboard renders this body verbatim, so the cause has to travel with it.
            return (StatusCode::CONFLICT, format!("{error:#}")).into_response();
        }
    } else {
        instance.view = View::Terminal;
        instance.acp_load_session_capable = None;
        // A later re-enable starts a fresh session/new.
        if instance.acp_session_id.is_some() {
            tracing::debug!(
                target: "acp.switch",
                session = %id,
                "clearing acp_session_id on disable"
            );
            instance.acp_session_id = None;
            instance.import_pending = None;
        }
    }

    instance = match persist_terminal_view(
        &state,
        &instance,
        keep_context,
        memory_expected,
        disk_expected,
        stop.clone(),
    )
    .await
    {
        Ok(emitted) => emitted,
        Err(response) => return response,
    };
    // Committed before shutdown so the reconciler cannot respawn a worker in
    // the teardown window. A kept-context switch still deletes the ACP
    // projection below, so it must not report success from an ungated stop:
    // `shutdown_and_require_dead` surfaces `TeardownPending` while the runner
    // is not proven dead.
    let shutdown_result = if keep_context {
        state
            .acp_supervisor
            .shutdown_and_require_dead(stop.clone())
            .await
    } else {
        // The pane is about to reopen on this checkout, so it is kept whatever
        // the row said: a launcher still starting is a live process inside it.
        state.acp_supervisor.shutdown_and_delete(stop.clone()).await
    };
    match shutdown_result {
        Ok(()) => {}
        Err(SupervisorError::TeardownPending(_)) => {
            // The view commit prevents respawn, but cannot itself prove death.
            return (
                StatusCode::CONFLICT,
                format!(
                    "Session {id} is not proven stopped; the switch to the terminal view is \
                     already committed, retry after its executions exit"
                ),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "Session {id} cleanup failed after the terminal view was committed: {error}"
                ),
            )
                .into_response()
        }
    }
    finish_terminal_switch(&state, &id, instance, stop, before_terminal).await
}

/// The tail of a disable: drop the ACP projection, forget the session, and
/// bring the tmux pane back. Best-effort by construction: the view switch is
/// already committed, so a failed event deletion must not strand the session
/// in a wedged state. Any residual transcript is left in place; only a purge that
/// still sees a structured session removes it.
/// purge path treats a post-commit sidecar failure.
async fn finish_terminal_switch(
    state: &Arc<AppState>,
    id: &str,
    mut instance: Instance,
    stop: Arc<crate::session::runner_journal::OwnedStop>,
    before_terminal: Arc<crate::session::LaunchOrigin>,
) -> Response {
    let original = stop.original_arc();
    let earlier = [original.clone(), before_terminal, stop.current_projection()];
    let store = state.acp_event_store.clone();
    let supervisor = state.acp_supervisor.clone();
    let cleanup = tokio::task::spawn_blocking(move || crate::session::runner_journal::finish_owned_stop(&stop, |row| {
        if let Err(error) = store.delete_session(&row.id) { tracing::warn!(target: "acp.switch", session = %row.id, %error, "ACP event deletion failed after the view switch"); }
        supervisor.forget_session(&original);
        Ok(())
    })).await;
    if !matches!(cleanup, Ok(Ok(()))) {
        return (
            StatusCode::CONFLICT,
            "original terminal handoff scope changed during completion; retry",
        )
            .into_response();
    }

    let started = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let (reserved, acknowledged) = instance.start_acknowledged()?;
        Ok((instance, reserved, acknowledged))
    })
    .await;
    match started {
        Ok(Ok((started, reserved, acknowledged))) => {
            let mut rows = state.instances.clone().write_owned().await;
            let publication = tokio::task::spawn_blocking(move || {
                acknowledged.with_storage(|_, _| {
                    let slot = rows
                        .iter_mut()
                        .find(|row| row.id == acknowledged.session_id())
                        .ok_or_else(|| {
                            anyhow::anyhow!("original Terminal launch cache row disappeared")
                        })?;
                    let cached = crate::session::LaunchOrigin::capture(slot)?;
                    anyhow::ensure!(
                        earlier
                            .iter()
                            .any(|ack| ack.recognizes_published_snapshot(&cached))
                            || reserved
                                .as_ref()
                                .is_some_and(|ack| ack.recognizes_published_snapshot(&cached))
                            || acknowledged.recognizes_published_snapshot(&cached),
                        "original Terminal launch cache row was superseded"
                    );
                    let started_at = started.last_start_time;
                    *slot = crate::server::reload::merge_runtime_fields(slot, started);
                    slot.last_start_time = started_at;
                    Ok(())
                })
            })
            .await;
            match publication {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return (
                        StatusCode::CONFLICT,
                        format!("original Terminal launch publication was superseded: {error:#}"),
                    )
                        .into_response()
                }
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("original Terminal launch publication task failed: {error}"),
                    )
                        .into_response()
                }
            }
        }
        // An archived or trashed session switches views without starting.
        Ok(Err(e)) if e.downcast_ref::<crate::session::StartBlocked>().is_some() => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "acp.switch", session = %id, "tmux start after disable: {e}");
        }
        Err(e) => {
            tracing::error!(target: "acp.switch", session = %id, "spawn_blocking failed: {e}");
        }
    }
    view_response(id.to_string(), View::Terminal)
}

/// Persist the terminal handoff with compare-and-swap guards on both cache and disk.
async fn persist_terminal_view(
    state: &AppState,
    instance: &Instance,
    keep_context: bool,
    memory_expected: HandoffSnapshot,
    disk_expected: HandoffSnapshot,
    stop: Arc<crate::session::runner_journal::OwnedStop>,
) -> Result<Instance, Response> {
    let instances = state.instances.write().await;
    let Some(slot) = instances.iter().find(|row| row.id == instance.id) else {
        return Err(session_not_found());
    };
    let adopted_matches = slot.view == disk_expected.0
        && slot.acp_session_id == disk_expected.1
        && (!keep_context || disk_expected.2.matches(slot));
    let original_matches = slot.view == memory_expected.0
        && slot.acp_session_id == memory_expected.1
        && (!keep_context || memory_expected.2.matches(slot));
    if !(original_matches || adopted_matches) {
        return Err((
            StatusCode::CONFLICT,
            "ACP identity changed during terminal handoff; retry",
        )
            .into_response());
    }
    drop(instances);

    let snapshot = instance.clone();
    let persisted_conversation = snapshot.conversation_state();
    let owner = stop.clone();
    let prior_projection = stop.current_projection();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<Instance> {
        owner.update_projection(|slot| {
            anyhow::ensure!(
                slot.view == disk_expected.0
                    && slot.acp_session_id == disk_expected.1
                    && (!keep_context || disk_expected.2.matches(slot)),
                "ACP identity changed during terminal handoff; retry"
            );
            slot.view = View::Terminal;
            slot.acp_session_id = snapshot.acp_session_id.clone();
            slot.import_pending = snapshot.import_pending;
            if keep_context {
                slot.adopt_conversation_state(persisted_conversation);
            }
            let mut emitted = slot.clone();
            emitted.storage_origin = Some(Arc::new(owner.storage().clone()));
            Ok(emitted)
        })
    })
    .await;
    let emitted = match save_result {
        Ok(Ok(emitted)) => emitted,
        Ok(Err(error)) => {
            return Err((
                StatusCode::CONFLICT,
                format!("terminal handoff was not saved: {error}"),
            )
                .into_response());
        }
        Err(join_error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("terminal handoff save failed: {join_error}"),
            )
                .into_response());
        }
    };

    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|row| {
        crate::session::LaunchOrigin::capture(row).is_ok_and(|cached| {
            stop.original().recognizes_published_snapshot(&cached)
                || prior_projection.recognizes_published_snapshot(&cached)
                || stop
                    .current_projection()
                    .recognizes_published_snapshot(&cached)
        })
    }) {
        *slot = crate::server::reload::merge_runtime_fields(slot, emitted.clone());
        slot.acp_load_session_capable = None;
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    } else {
        tracing::warn!(
            target: "acp.switch",
            session = %instance.id,
            "session missing from cache after terminal handoff save; continuing teardown"
        );
    }
    Ok(emitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_structured_seed_covers_import_direction_b_and_fresh() {
        // (tool, agent, acp id, agent id, import_pending, transcript, expected id, replay)
        let cases = [
            (
                "claude",
                "claude",
                Some("acp-1"),
                Some("agent-1"),
                true,
                true,
                Some("acp-1"),
                true,
            ),
            (
                "claude",
                "claude",
                Some("acp-1"),
                None,
                false,
                true,
                Some("acp-1"),
                false,
            ),
            (
                "claude",
                "claude",
                None,
                Some("agent-1"),
                false,
                true,
                Some("agent-1"),
                true,
            ),
            // Transcript confirmed absent.
            (
                "claude",
                "claude",
                None,
                Some("agent-1"),
                false,
                false,
                None,
                false,
            ),
            // Non-resumable pairings.
            (
                "claude",
                "codex",
                None,
                Some("agent-1"),
                false,
                true,
                None,
                false,
            ),
            (
                "codex",
                "codex",
                None,
                Some("agent-1"),
                false,
                true,
                None,
                false,
            ),
            ("claude", "claude", None, None, false, true, None, false),
        ];
        for (tool, agent, acp, agent_sid, import, present, expected, replay) in cases {
            let s = resolve_structured_seed(tool, agent, acp, agent_sid, import, present);
            assert_eq!(s.stored_acp_session_id.as_deref(), expected);
            assert_eq!(s.seed_history_replay, replay);
            assert_eq!(
                s.import_terminal,
                acp.is_none() && expected == agent_sid && replay,
            );
        }
    }

    #[test]
    fn persisted_disable_adoption_keeps_nondefault_launch_profile() {
        let mut cached = Instance::new("t", "/tmp");
        cached.source_profile = "work".to_string();
        let mut persisted = cached.clone();
        persisted.source_profile.clear();
        persisted.view = View::Structured;

        let adopted = adopt_persisted_structured_instance(cached, persisted, "work");
        assert_eq!(adopted.effective_profile(), "work");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_disable_requires_history_proof_even_without_a_worker_or_projection() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut instance = Instance::new("uncovered-terminal", project.to_str().unwrap());
        instance.source_profile = "disable-history".into();
        instance.view = View::Terminal;
        instance.tool = "shell".into();
        instance.runner_journal = Default::default();
        let id = instance.id.clone();
        let storage = crate::session::Storage::new_unwatched("disable-history").unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![instance]);
        let response = acp_disable(State(state), Path(id.clone()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let durable = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(durable.view, View::Terminal);
    }
}

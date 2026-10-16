//! Switching a session between the terminal (tmux) and structured views.

use anyhow::Context as _;
use serde::Serialize;

use crate::server::api::{find_instance, instance_exists};
use crate::server::push::publish_status_change;
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
    cached: Instance,
    mut persisted: Instance,
    source_profile: &str,
) -> Instance {
    persisted.source_profile = source_profile.to_owned();
    crate::server::reload::merge_runtime_fields(cached, persisted)
}

pub async fn acp_enable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
    if instance.terminal_handoff_pending {
        return (
            StatusCode::CONFLICT,
            "terminal handoff is still pending; retry disabling the structured view first",
        )
            .into_response();
    }
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
    if let Err(resp) =
        commit_structured_view(&state, &mut instance, selected_binding.as_ref()).await
    {
        return resp;
    }
    spawn_enabled_worker(
        state.clone(),
        inst_lock.clone(),
        id.clone(),
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
) -> Result<(), Response> {
    let inst_for_transition = instance.clone();
    let profile = instance.source_profile.clone();
    let file_watch = state.file_watch.clone();
    let binding_for_transition = selected_binding.cloned();
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let storage = crate::session::Storage::open(&profile, file_watch)?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&inst_for_transition.id)
            .map_err(|error| {
                anyhow::anyhow!("failed to acquire terminal-to-ACP lifecycle lock: {error}")
            })?;
        let mut current = storage
            .load()?
            .into_iter()
            .find(|row| row.id == inst_for_transition.id)
            .context("session disappeared before terminal-to-ACP admission")?;
        current.source_profile = profile;
        current.ensure_startable()?;
        let previous_generation = current.lifecycle_generation;
        let previous_reservation = current.lifecycle_reservation.clone();
        let generation = current.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Launch,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        current.kill_locked(&storage).with_context(|| {
            format!(
                "could not settle terminal session '{}' before switching to structured view",
                current.title
            )
        })?;
        current.kill_ancillary_tmux_sessions_locked();
        // Still the terminal row here, so this stops the pane's scope; the
        // worker spawns after the transition, under a fresh unit name.
        current.release_scope();
        storage.update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == inst_for_transition.id)
            else {
                anyhow::bail!("session disappeared during terminal-to-ACP transition");
            };
            slot.ensure_startable()?;
            anyhow::ensure!(
                slot.lifecycle_generation == previous_generation
                    && slot.lifecycle_reservation == previous_reservation,
                "session lifecycle ownership changed during terminal-to-ACP transition"
            );
            let committed_generation = slot.try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )?;
            anyhow::ensure!(
                committed_generation == generation,
                "terminal-to-ACP generation changed"
            );
            slot.view = View::Structured;
            slot.resume_intent = ResumeIntent::Default;
            if let Some(binding) = &binding_for_transition {
                slot.agent_session_id = Some(binding.session_id.clone());
                slot.agent_session_binding = Some(binding.clone());
            }
            // Structured status is event-driven and the tmux poller skips
            // structured rows, so a Running/Waiting status would never settle.
            slot.status = Status::Idle;
            slot.release_lifecycle_reservation_if_owned(
                crate::session::LifecycleOperation::Launch,
                generation,
            );
            Ok(slot.lifecycle_generation)
        })
    })
    .await;
    let id = instance.id.clone();
    let lifecycle_generation = match transition {
        Ok(Ok(generation)) => generation,
        Ok(Err(error)) => {
            if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
                return Err(crate::server::api::start_blocked_response(*blocked));
            }
            if error
                .downcast_ref::<crate::session::LifecycleReservationError>()
                .is_some()
            {
                return Err((StatusCode::CONFLICT, error.to_string()).into_response());
            }
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
        inst.lifecycle_reservation = None;
        inst.acp_load_session_capable = None;
    };
    apply(instance);
    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|candidate| candidate.id == id) {
        if lifecycle_generation >= slot.lifecycle_generation {
            let old_status = slot.status;
            apply(slot);
            publish_status_change(&state.status_tx, slot, old_status);
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
    session_id: String,
    agent_name: String,
    selected_conversation: Option<(String, crate::session::ExecutionBinding)>,
) {
    tokio::spawn(async move {
        // Held through the spawn so a following disable cannot tear down
        // first and then be undone by this late task.
        let _transition_guard = inst_lock.lock().await;
        let supervisor = &state.acp_supervisor;
        let request = match deferred_enable_request(
            &state,
            &session_id,
            agent_name.clone(),
            selected_conversation,
        )
        .await
        {
            None => return,
            Some(Ok(request)) => request,
            Some(Err(message)) => {
                supervisor.publish_startup_error(&session_id, message);
                return;
            }
        };
        if let Err(e) = supervisor
            .spawn(
                request,
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            let message = structured_spawn_error_message(&e, &agent_name);
            tracing::warn!(target: "acp.switch", session = %session_id, "spawn after enable: {message}");
            supervisor.publish_startup_error(&session_id, message);
        }
    });
}

/// Built from the live row under the transition lock, since a provider switch
/// may commit while the enable waits. `None` once the session left the
/// structured view.
async fn deferred_enable_request(
    state: &AppState,
    session_id: &str,
    agent_name: String,
    selected_conversation: Option<(String, crate::session::ExecutionBinding)>,
) -> Option<Result<SpawnRequest, String>> {
    let instance = find_instance(state, session_id)
        .await
        .filter(Instance::is_structured)?;
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
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session_locked(
        &state.instances,
        &state.mutation_epoch,
        session_id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!(target: "acp.switch", session = %session_id, "container ensure failed: {e}");
            return Some(Err(format!("container start failed: {e}")));
        }
    };
    Some(Ok(SpawnRequest {
        stored_acp_session_id: seed.stored_acp_session_id,
        seed_history_replay: seed.seed_history_replay,
        sandbox_continuation: if seed.import_terminal {
            crate::acp::supervisor::SandboxContinuation::ImportTerminal
        } else {
            crate::acp::supervisor::SandboxContinuation::Persisted
        },
        claude_store_pin,
        ..spawn_request_for(&instance, agent_name, sandbox_info)
    }))
}

/// Switch a structured session back to tmux. When the agent shares a
/// CLI-resumable transcript and an ACP session id exists, the conversation is
/// kept and tmux resumes it (#2252); otherwise the transcript is deleted.
pub async fn acp_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
    let profile = instance.source_profile.clone();
    let memory_expected = (
        instance.view,
        instance.acp_session_id.clone(),
        instance.conversation_state(),
    );

    let mut terminal_handoff_retry = false;
    if !instance.is_structured() {
        // A reload may have cached a pre-enable terminal snapshot; trust the
        // durable row before answering idempotently.
        match crate::server::api::load_persisted_instance(&state, &profile, &id).await {
            Ok(Some(durable)) if durable.is_structured() => {
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
            Ok(Some(durable)) if durable.terminal_handoff_pending => {
                instance = durable;
                terminal_handoff_retry = true;
            }
            Ok(Some(_)) => return view_response(id, View::Terminal),
            Ok(None) => return session_not_found(),
            Err(resp) => return resp,
        }
    }

    let disk_expected = (
        instance.view,
        instance.acp_session_id.clone(),
        instance.conversation_state(),
    );

    let keep_context = if terminal_handoff_retry {
        // The pending ACP id is the persisted non-destructive shutdown decision.
        instance.acp_session_id.is_some()
    } else {
        // Resolve the active adapter: switch_acp_agent can point away from the default.
        let acp_agent = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
        crate::agents::acp_transcript_cli_resumable(&instance.tool, &acp_agent)
            && instance.acp_session_id.is_some()
    };
    if !terminal_handoff_retry {
        if keep_context {
            tracing::debug!(
                target: "acp.switch",
                session = %id,
                "keeping context on disable: carrying acp_session_id into agent_session_id for claude --resume"
            );
            let pending_acp_session_id = instance.acp_session_id.clone();
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
            // A canceled request must retry the same non-destructive shutdown decision.
            instance.acp_session_id = pending_acp_session_id;
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

        instance.terminal_handoff_pending = true;
        if let Err(response) = persist_terminal_view(
            &state,
            &instance,
            &profile,
            keep_context,
            memory_expected,
            disk_expected,
        )
        .await
        {
            return response;
        }
    }

    // Committed before shutdown so the reconciler cannot respawn a worker in
    // the teardown window.
    let shutdown_result = if keep_context {
        state.acp_supervisor.shutdown(&id).await
    } else {
        state.acp_supervisor.shutdown_and_delete(&id).await
    };
    match shutdown_result {
        Ok(()) | Err(SupervisorError::UnknownSession(_)) => {}
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                format!("structured worker shutdown failed: {error}"),
            )
                .into_response()
        }
    }
    match crate::process::worker_registry::load_recovery_candidate(&id) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                "structured worker shutdown remains unsettled; retry after cleanup completes",
            )
                .into_response()
        }
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                format!("cannot verify structured worker settlement: {error:#}"),
            )
                .into_response()
        }
    }
    // The tmux pane reprints a kept conversation, so the ACP projection goes.
    state.acp_supervisor.forget_session(&id);
    if let Err(error) = state.acp_event_store.delete_session_fallible(&id) {
        return (
            StatusCode::CONFLICT,
            format!("terminal handoff transcript cleanup failed: {error:#}"),
        )
            .into_response();
    }
    instance.terminal_handoff_completion = true;

    let start_result = tokio::task::spawn_blocking(move || instance.start()).await;
    match start_result {
        Ok(Ok(())) => {}
        // A shelved session switches views without starting.
        Ok(Err(error))
            if matches!(
                error.downcast_ref::<crate::session::StartBlocked>(),
                Some(
                    crate::session::StartBlocked::Archived | crate::session::StartBlocked::Trashed
                )
            ) => {}
        Ok(Err(error)) => {
            return (
                StatusCode::CONFLICT,
                format!("terminal start after structured shutdown failed: {error:#}"),
            )
                .into_response()
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("terminal start task failed: {error}"),
            )
                .into_response()
        }
    }
    if let Err(response) = complete_terminal_handoff(&state, &profile, &id).await {
        return response;
    }
    view_response(id, View::Terminal)
}

/// Persist the terminal handoff with compare-and-swap guards on both cache and disk.
async fn persist_terminal_view(
    state: &AppState,
    instance: &Instance,
    profile: &str,
    keep_context: bool,
    memory_expected: HandoffSnapshot,
    disk_expected: HandoffSnapshot,
) -> Result<(), Response> {
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
    let profile_for_save = profile.to_string();
    let file_watch = state.file_watch.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::open(&profile_for_save, file_watch)?;
        storage.update(|all, _groups| {
            let Some(slot) = all.iter_mut().find(|candidate| candidate.id == snapshot.id) else {
                anyhow::bail!("session disappeared during terminal handoff");
            };
            anyhow::ensure!(
                slot.view == disk_expected.0
                    && slot.acp_session_id == disk_expected.1
                    && (!keep_context || disk_expected.2.matches(slot)),
                "ACP identity changed during terminal handoff; retry"
            );
            slot.view = View::Terminal;
            slot.acp_session_id = snapshot.acp_session_id.clone();
            slot.terminal_handoff_pending = snapshot.terminal_handoff_pending;
            slot.import_pending = snapshot.import_pending;
            if keep_context {
                slot.adopt_conversation_state(persisted_conversation.clone());
            }
            Ok(())
        })?;
        Ok(())
    })
    .await;
    match save_result {
        Ok(Ok(())) => {}
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
    }

    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|row| row.id == instance.id) {
        slot.view = View::Terminal;
        slot.acp_load_session_capable = None;
        slot.acp_session_id = instance.acp_session_id.clone();
        slot.terminal_handoff_pending = instance.terminal_handoff_pending;
        slot.import_pending = instance.import_pending;
        if keep_context {
            slot.adopt_conversation_state(instance.conversation_state());
        }
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
    Ok(())
}

async fn complete_terminal_handoff(
    state: &AppState,
    profile: &str,
    id: &str,
) -> Result<(), Response> {
    let profile = profile.to_string();
    let id_for_save = id.to_string();
    let file_watch = state.file_watch.clone();
    let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::open(&profile, file_watch)?;
        storage.update(|rows, _groups| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == id_for_save)
                .context("session disappeared while completing terminal handoff")?;
            anyhow::ensure!(
                row.view == View::Terminal,
                "session view changed while completing terminal handoff"
            );
            row.terminal_handoff_pending = false;
            row.acp_session_id = None;
            Ok(())
        })
    })
    .await;
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return Err((
                StatusCode::CONFLICT,
                format!("terminal handoff completion was not saved: {error:#}"),
            )
                .into_response())
        }
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("terminal handoff completion task failed: {error}"),
            )
                .into_response())
        }
    }
    let mut instances = state.instances.write().await;
    if let Some(row) = instances.iter_mut().find(|row| row.id == id) {
        row.terminal_handoff_pending = false;
        row.acp_session_id = None;
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An enable queued behind the transition lock must spawn on a provider
    /// switch that committed while it waited, not on the row it was queued with.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_queued_enable_spawns_on_the_live_provider() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("claude", "/tmp/aoe-queued-enable");
        inst.view = View::Structured;
        inst.agent_name = Some("claude".to_string());
        let id = inst.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![inst.clone()]);
        let (tx, mut routed) = tokio::sync::mpsc::unbounded_channel();
        let launcher: crate::acp::supervisor::Launcher = Arc::new(move |config, _id| {
            let _ = tx.send(config.provider_routing);
            Box::pin(async {
                Err(crate::acp::acp_client::AcpError::Spawn(
                    "test launcher".into(),
                ))
            })
        });
        let state =
            crate::server::test_support::build_test_app_state_with_launcher(vec![inst], launcher);

        let inst_lock = state.instance_lock(&id).await;
        let guard = inst_lock.lock().await;
        spawn_enabled_worker(
            state.clone(),
            inst_lock.clone(),
            id.clone(),
            "claude".to_string(),
            None,
        );
        if let Some(row) = state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|i| i.id == id)
        {
            row.agent_provider = Some("vertex".to_string());
        }
        drop(guard);

        let routing = tokio::time::timeout(std::time::Duration::from_secs(10), routed.recv())
            .await
            .expect("the queued enable reached the launcher")
            .expect("launcher alive");
        assert!(
            routing.contains(&("CLAUDE_CODE_USE_VERTEX".to_string(), "1".to_string())),
            "{routing:?}"
        );
    }

    /// The terminal-to-ACP switch settles the live slot to Idle itself. A plugin that last heard
    /// Running would never learn the session went Idle unless the switch publishes the move.
    #[tokio::test]
    #[serial_test::serial]
    async fn switching_to_structured_view_publishes_the_idle_it_settles_to() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("switch-notify", "/tmp/aoe-switch-notify");
        inst.source_profile = "default".to_string();
        inst.status = Status::Running;
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![inst.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
        let mut rx = state.status_tx.subscribe();
        let mut working = inst.clone();

        assert!(commit_structured_view(&state, &mut working, None)
            .await
            .is_ok());

        let change = rx.try_recv().expect("the live slot's move is published");
        assert_eq!(change.instance_id, inst.id);
        assert_eq!((change.old, change.new), (Status::Running, Status::Idle));
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn stale_enable_preserves_failed_irreversible_purge_authority() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        use crate::session::LifecycleOperation;

        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let mut stale = Instance::new("stale-enable", root.path().to_str().unwrap());
        stale.source_profile = "default".into();
        stale.status = Status::Idle;
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![stale.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![stale.clone()]);
        let storage = crate::session::Storage::open_unwatched("default").unwrap();
        let transaction = match PurgeTransaction::reserve_with_acp_transcript(
            storage.clone(),
            DeletionRequest {
                session_id: stale.id.clone(),
                instance: stale.clone(),
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        let failure = transaction
            .run_hooks()
            .begin_irreversible()
            .unwrap()
            .finish_with_transcript_cleanup(|_| anyhow::bail!("transcript unavailable"));
        assert!(!failure.success && failure.teardown_started);
        let durable = storage.load().unwrap().remove(0);
        let generation = durable.lifecycle_generation;
        let journal = crate::session::lifecycle_journal::path_for(
            storage.sessions_path(),
            &stale.id,
            generation,
        );
        let row_before = std::fs::read(storage.sessions_path()).unwrap();
        let journal_before = std::fs::read(&journal).unwrap();
        assert!(
            stale.ensure_startable().is_ok(),
            "cached admission must be stale"
        );
        let response = commit_structured_view(&state, &mut stale, None)
            .await
            .expect_err("stale enable overwrote irreversible purge");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), row_before);
        assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.status, Status::Deleting);
        assert_eq!(durable.view, View::Terminal);
        assert!(durable.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn switching_to_structured_view_stops_on_unproven_native_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let runtime =
            crate::process::prime::PrimeRuntime::new("/bin/false".into(), root.path().into(), None)
                .unwrap();
        runtime.prepare().unwrap();
        runtime.adopt_prepared().unwrap();
        std::fs::write(runtime.directory.join("frontend.identity"), "invalid").unwrap();
        let mut instance = Instance::new("Prime", root.path().to_str().unwrap());
        instance.source_profile = "default".into();
        instance.active_execution = Some(crate::session::ActiveExecution {
            launch_id: "launch".into(),
            binding: crate::session::ExecutionBinding {
                agent: "prime-agent".into(),
                stores: Vec::new(),
                configuration: Vec::new(),
                cwd: root.path().into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            },
            capture: Some(crate::session::CaptureContext::PrimeStatus(runtime.clone())),
            container: None,
        });
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);

        assert!(commit_structured_view(&state, &mut instance, None)
            .await
            .is_err());

        let stored = crate::session::Storage::new_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(stored.view, View::Terminal);
        assert!(runtime.directory.exists());
        std::fs::remove_dir_all(&runtime.directory).unwrap();
    }

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
    async fn persisted_terminal_handoff_finishes_after_worker_evidence_disappears() {
        let _app = crate::session::test_support::isolate_app_dir();
        let mut instance = Instance::new("pending handoff", "/tmp/aoe-pending-handoff");
        instance.view = View::Terminal;
        instance.terminal_handoff_pending = true;
        instance.archived_at = Some(chrono::Utc::now());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance]);

        let response = acp_disable(State(state), Path(id.clone()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let stored = crate::session::Storage::new_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(!stored.terminal_handoff_pending);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persisted_keep_context_handoff_retry_never_deletes_adapter_state() {
        let _app = crate::session::test_support::isolate_app_dir();
        let sid = "kept-acp-conversation";
        let mut instance = Instance::new("pending keep-context handoff", "/tmp");
        instance.tool = "claude".to_string();
        instance.agent_name = Some("claude".to_string());
        instance.view = View::Structured;
        instance.acp_session_id = Some(sid.to_string());
        let binding = crate::session::ConversationBinding {
            session_id: sid.to_string(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".to_string(),
                stores: vec!["/tmp/claude-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".to_string(),
                filesystem: "host".to_string(),
                exported_default_store: None,
            }),
            provenance: crate::session::ConversationProvenance::Asserted,
            transcript_path: None,
        };
        instance.resume_intent = crate::session::ResumeIntent::Use(sid.to_string());
        instance.resume_binding = Some(binding);
        instance.switch_to_terminal_keep_context(None).unwrap();
        instance.acp_session_id = Some(sid.to_string());
        instance.terminal_handoff_pending = true;
        instance.archived_at = Some(chrono::Utc::now());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance]);
        let commands = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        let dead_pid = {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let mut record = crate::process::worker_registry::WorkerRecord::new(
            id.clone(),
            dead_pid,
            crate::process::worker_registry::socket_path_for(&id).unwrap(),
            "claude-agent-acp".to_string(),
            "claude-code".to_string(),
            "/tmp".into(),
            None,
            vec![],
            vec![],
            Some(sid.to_string()),
            Some("default".to_string()),
        );
        record.stored_acp_session_id = Some(sid.to_string());
        crate::process::worker_registry::save(&record).unwrap();

        let response = acp_disable(State(Arc::clone(&state)), Path(id.clone()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !commands
                .lock()
                .expect("command recorder poisoned")
                .contains(&"delete_session"),
            "keep-context retry deleted the preserved adapter conversation"
        );
        let stored = crate::session::Storage::new_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(!stored.terminal_handoff_pending);
        assert_eq!(stored.acp_session_id, None);
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("kept-acp-conversation")
        );
    }
}

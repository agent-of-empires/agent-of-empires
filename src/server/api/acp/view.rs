//! Switching a session between the terminal (tmux) and structured views.

use serde::Serialize;

use crate::server::api::{find_instance, instance_exists};
use crate::server::push::publish_status_change;
use crate::session::{Instance, ResumeIntent, StartBlocked, Status, View};

use super::*;

#[derive(Debug, Serialize)]
pub struct ViewSwitchResponse {
    pub session_id: String,
    pub view: View,
}

type HandoffSnapshot = (View, Option<String>, crate::session::ConversationState);

#[derive(Debug)]
struct StructuredViewConflict(&'static str);

impl std::fmt::Display for StructuredViewConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StructuredViewConflict {}

fn validate_structured_view_transition(
    durable: &Instance,
    expected_generation: u64,
) -> anyhow::Result<()> {
    if durable.status == Status::Deleting {
        return Err(anyhow::Error::new(StructuredViewConflict(
            "session is being purged",
        )));
    }

    let now = chrono::Utc::now();
    let purge_reserved = durable.has_fresh_lifecycle_reservation(now)
        && durable
            .lifecycle_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.op == crate::session::LifecycleOperation::Purge);
    if purge_reserved {
        return Err(anyhow::Error::new(StructuredViewConflict(
            "session has an active purge reservation",
        )));
    }
    if durable.lifecycle_generation != expected_generation {
        return Err(anyhow::Error::new(StructuredViewConflict(
            "session lifecycle changed; reload and retry the view switch",
        )));
    }
    if let Err(blocked) = durable.ensure_startable() {
        return Err(anyhow::Error::new(blocked));
    }
    Ok(())
}

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
    if instance.acp_handoff_pending {
        return crate::server::api::start_blocked_response(StartBlocked::AcpHandoffPending);
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
        let durable = storage
            .load()?
            .into_iter()
            .find(|candidate| candidate.id == inst_for_transition.id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared during terminal-to-ACP transition"))?;
        validate_structured_view_transition(&durable, inst_for_transition.lifecycle_generation)?;
        if let Err(e) = inst_for_transition.kill_locked() {
            tracing::warn!(target: "acp.switch", session = %inst_for_transition.id, "kill tmux failed: {e}");
        }
        inst_for_transition.kill_ancillary_tmux_sessions_locked();
        // Still the terminal row here, so this stops the pane's scope; the
        // worker spawns after the transition, under a fresh unit name.
        inst_for_transition.release_scope();
        storage.update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == inst_for_transition.id)
            else {
                anyhow::bail!("session disappeared during terminal-to-ACP transition");
            };
            validate_structured_view_transition(slot, inst_for_transition.lifecycle_generation)?;
            slot.view = View::Structured;
            slot.resume_intent = ResumeIntent::Default;
            if let Some(binding) = &binding_for_transition {
                slot.agent_session_id = Some(binding.session_id.clone());
                slot.agent_session_binding = Some(binding.clone());
            }
            // Structured status is event-driven and the tmux poller skips
            // structured rows, so a Running/Waiting status would never settle.
            slot.status = Status::Idle;
            slot.lifecycle_generation = slot.lifecycle_generation.saturating_add(1);
            Ok(slot.lifecycle_generation)
        })
    })
    .await;
    let id = instance.id.clone();
    let lifecycle_generation = match transition {
        Ok(Ok(generation)) => generation,
        Ok(Err(error)) => {
            if let Some(blocked) = error.downcast_ref::<StartBlocked>() {
                return Err(crate::server::api::start_blocked_response(*blocked));
            }
            if let Some(conflict) = error.downcast_ref::<StructuredViewConflict>() {
                return Err((StatusCode::CONFLICT, conflict.0).into_response());
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

    if !instance.is_structured() {
        // A reload may have cached a pre-enable terminal snapshot; trust the
        // durable row before answering idempotently.
        match crate::server::api::load_persisted_instance(&state, &profile, &id).await {
            Ok(Some(durable)) if durable.is_structured() => {
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
            Ok(Some(mut durable)) if durable.acp_handoff_pending => {
                durable.source_profile = profile;
                let keep_context = durable.acp_session_id.is_some();
                if let Err(response) =
                    complete_terminal_handoff(&state, durable, keep_context).await
                {
                    return response;
                }
                return view_response(id, View::Terminal);
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
    instance.acp_handoff_pending = true;

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
    if let Err(response) = complete_terminal_handoff(&state, instance, keep_context).await {
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
            slot.import_pending = snapshot.import_pending;
            slot.acp_handoff_pending = true;
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
        slot.import_pending = instance.import_pending;
        slot.acp_handoff_pending = true;
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
    state: &Arc<AppState>,
    mut instance: Instance,
    keep_context: bool,
) -> Result<(), Response> {
    let id = instance.id.clone();
    let timeout = std::time::Duration::from_secs(5);
    let shutdown = if keep_context {
        state.acp_supervisor.shutdown_and_wait(&id, timeout).await
    } else {
        state
            .acp_supervisor
            .shutdown_and_delete_and_wait(&id, timeout)
            .await
    };
    if let Err(error) = shutdown {
        tracing::warn!(target: "acp.switch", session = %id, "shutdown structured view remains pending: {error}");
        return Err((
            StatusCode::CONFLICT,
            format!("structured worker has not settled; retry the terminal handoff: {error}"),
        )
            .into_response());
    }

    state.acp_supervisor.forget_session(&id);
    if let Err(error) = state.acp_event_store.delete_session_fallible(&id) {
        tracing::warn!(target: "acp.switch", session = %id, "structured event projection cleanup remains pending: {error:#}");
        return Err((
            StatusCode::CONFLICT,
            "terminal handoff cleanup remains pending",
        )
            .into_response());
    }

    let profile = instance.source_profile.clone();
    let persisted_id = id.clone();
    let file_watch = state.file_watch.clone();
    let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::open(&profile, file_watch)?;
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&persisted_id)?;
        storage.update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == persisted_id)
            else {
                anyhow::bail!("session disappeared while completing terminal handoff");
            };
            anyhow::ensure!(
                slot.view == View::Terminal && slot.acp_handoff_pending,
                "terminal handoff was superseded"
            );
            slot.acp_handoff_pending = false;
            Ok(())
        })?;
        Ok(())
    })
    .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(target: "acp.switch", session = %id, "failed to clear terminal handoff marker: {error:#}");
            return Err((
                StatusCode::CONFLICT,
                format!("terminal handoff cleanup remains pending: {error:#}"),
            )
                .into_response());
        }
        Err(error) => {
            tracing::warn!(target: "acp.switch", session = %id, "terminal handoff marker task failed: {error}");
            return Err((
                StatusCode::CONFLICT,
                "terminal handoff cleanup remains pending",
            )
                .into_response());
        }
    }

    if let Some(slot) = state
        .instances
        .write()
        .await
        .iter_mut()
        .find(|candidate| candidate.id == id)
    {
        slot.acp_handoff_pending = false;
    }
    instance.acp_handoff_pending = false;

    match tokio::task::spawn_blocking(move || instance.start()).await {
        Ok(Ok(())) => {}
        Ok(Err(error))
            if error
                .downcast_ref::<crate::session::StartBlocked>()
                .is_some() => {}
        Ok(Err(error)) => {
            tracing::warn!(target: "acp.switch", session = %id, "tmux start after disable: {error}");
        }
        Err(error) => {
            tracing::error!(target: "acp.switch", session = %id, "spawn_blocking failed: {error}");
        }
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
    async fn structured_view_rejects_deleting_session_with_purge_reservation() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("switch-during-purge", "/tmp/aoe-switch-during-purge");
        inst.source_profile = "default".to_string();
        inst.view = View::Terminal;
        inst.status = Status::Deleting;
        inst.lifecycle_generation = 7;
        inst.lifecycle_reservation = Some(crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Purge,
            generation: 7,
            at: chrono::Utc::now(),
        });
        let id = inst.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![inst.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
        let mut working = inst;

        let response = commit_structured_view(&state, &mut working, None)
            .await
            .expect_err("a purge reservation must fence the view transition");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(working.view, View::Terminal);
        assert_eq!(working.status, Status::Deleting);
        assert_eq!(working.lifecycle_generation, 7);
        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .expect("purging row remains stored");
        assert_eq!(stored.view, View::Terminal);
        assert_eq!(stored.status, Status::Deleting);
        assert_eq!(stored.lifecycle_generation, 7);
        assert!(stored.lifecycle_reservation.is_some());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn structured_view_rejects_durable_unstartable_state_from_stale_cache() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut cached_archived =
            Instance::new("archived-stale-cache", "/tmp/aoe-archived-stale-cache");
        cached_archived.source_profile = "default".to_string();
        cached_archived.view = View::Terminal;
        let mut durable_archived = cached_archived.clone();
        durable_archived.archive();

        let mut cached_trashed =
            Instance::new("trashed-stale-cache", "/tmp/aoe-trashed-stale-cache");
        cached_trashed.source_profile = "default".to_string();
        cached_trashed.view = View::Terminal;
        let mut durable_trashed = cached_trashed.clone();
        durable_trashed.trash();

        let mut cached_handoff =
            Instance::new("handoff-stale-cache", "/tmp/aoe-handoff-stale-cache");
        cached_handoff.source_profile = "default".to_string();
        cached_handoff.view = View::Terminal;
        let mut durable_handoff = cached_handoff.clone();
        durable_handoff.acp_handoff_pending = true;

        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![
                durable_archived.clone(),
                durable_trashed.clone(),
                durable_handoff.clone(),
            ],
        );
        let state = crate::server::test_support::build_test_app_state(vec![
            cached_archived.clone(),
            cached_trashed.clone(),
            cached_handoff.clone(),
        ]);

        for (mut stale_cache, expected_error) in [
            (cached_archived, "session_archived"),
            (cached_trashed, "session_trashed"),
            (cached_handoff, "acp_handoff_pending"),
        ] {
            let response = commit_structured_view(&state, &mut stale_cache, None)
                .await
                .expect_err("durable archived or handoff-pending rows must block enable");
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(stale_cache.view, View::Terminal);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], expected_error);
        }

        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap();
        assert!(stored
            .iter()
            .find(|row| row.id == durable_archived.id)
            .expect("archived row remains stored")
            .is_archived());
        assert!(stored
            .iter()
            .find(|row| row.id == durable_trashed.id)
            .expect("trashed row remains stored")
            .is_trashed());
        assert!(
            stored
                .iter()
                .find(|row| row.id == durable_handoff.id)
                .expect("handoff row remains stored")
                .acp_handoff_pending
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unproven_shutdown_keeps_terminal_handoff_pending_and_projection() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance = Instance::new("handoff-pending", "/tmp/aoe-handoff-pending");
        instance.source_profile = "default".into();
        instance.view = View::Terminal;
        instance.acp_handoff_pending = true;
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        state
            .acp_event_store
            .record(&id, 1, &crate::acp::state::Event::ThinkingStarted)
            .unwrap();

        let record_path = crate::process::worker_registry::record_path(&id).unwrap();
        std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
        std::fs::create_dir(&record_path).unwrap();

        let response = complete_terminal_handoff(&state, instance, false)
            .await
            .expect_err("unreadable worker ownership must keep the handoff pending");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(state.acp_event_store.highest_seq(&id), 1);
        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .expect("terminal row remains stored");
        assert!(stored.acp_handoff_pending);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn settled_terminal_handoff_clears_pending_marker_from_storage_and_projection() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance = Instance::new("handoff-settled", "/tmp/aoe-handoff-settled");
        instance.source_profile = "default".into();
        instance.view = View::Terminal;
        instance.acp_handoff_pending = true;
        // Keep the post-handoff restart from launching a tmux pane in this test.
        instance.archived_at = Some(chrono::Utc::now());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);

        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        assert!(crate::process::worker_registry::load(&id)
            .unwrap()
            .is_none());

        complete_terminal_handoff(&state, instance, false)
            .await
            .expect("confirmed worker settlement should complete the terminal handoff");

        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent,
            "shutdown settlement must leave no supervisor worker"
        );
        assert!(
            crate::process::worker_registry::load(&id)
                .unwrap()
                .is_none(),
            "shutdown settlement must leave no worker ownership record"
        );

        let cached = state
            .instances
            .read()
            .await
            .iter()
            .find(|candidate| candidate.id == id)
            .cloned()
            .expect("terminal row remains in the cache");
        assert!(!cached.acp_handoff_pending);

        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .expect("terminal row remains stored");
        assert!(!stored.acp_handoff_pending);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn projection_delete_failure_keeps_terminal_handoff_pending_for_retry() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance =
            Instance::new("handoff-delete-failure", "/tmp/aoe-handoff-delete-failure");
        instance.source_profile = "default".into();
        instance.view = View::Terminal;
        instance.acp_handoff_pending = true;
        instance.archived_at = Some(chrono::Utc::now());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let mut state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
        let event_store =
            Arc::new(crate::acp::event_store::EventStore::open(&db_path, 100).unwrap());
        Arc::get_mut(&mut state)
            .expect("test state has one owner")
            .acp_event_store = event_store.clone();
        event_store
            .record(&id, 1, &crate::acp::state::Event::ThinkingStarted)
            .unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_acp_event_delete BEFORE DELETE ON acp_events \
             BEGIN SELECT RAISE(ABORT, 'test projection delete failure'); END;",
        )
        .unwrap();

        let response = complete_terminal_handoff(&state, instance.clone(), false)
            .await
            .expect_err("projection cleanup failure must leave the handoff pending");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(event_store.highest_seq(&id), 1);
        let cached = state
            .instances
            .read()
            .await
            .iter()
            .find(|candidate| candidate.id == id)
            .cloned()
            .expect("terminal row remains in the cache");
        assert!(cached.acp_handoff_pending);
        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .expect("terminal row remains stored");
        assert!(stored.acp_handoff_pending);

        conn.execute_batch("DROP TRIGGER fail_acp_event_delete")
            .unwrap();
        complete_terminal_handoff(&state, instance, false)
            .await
            .expect("handoff should complete after projection cleanup recovers");

        assert_eq!(event_store.highest_seq(&id), 0);
        let cached = state
            .instances
            .read()
            .await
            .iter()
            .find(|candidate| candidate.id == id)
            .cloned()
            .expect("terminal row remains in the cache");
        assert!(!cached.acp_handoff_pending);
        let stored = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == id)
            .expect("terminal row remains stored");
        assert!(!stored.acp_handoff_pending);
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
}

//! Switching a session between the terminal (tmux) and structured views.

use serde::Serialize;

use crate::server::api::{find_instance, instance_exists};
use crate::session::{Instance, ResumeIntent, SessionStore, Status, View};

use super::*;

#[derive(Debug, Serialize)]
pub struct ViewSwitchResponse {
    pub session_id: String,
    pub view: View,
}

type HandoffSnapshot = (View, Option<String>, crate::session::ConversationState);

async fn view_response(state: &Arc<AppState>, session_id: String, view: View) -> Response {
    let published = match state.runtime.publish(state).await {
        Ok(published) => published,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(row) = published
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == session_id)
    else {
        return session_not_found();
    };
    if row.view != view {
        return StatusCode::CONFLICT.into_response();
    }
    crate::server::runtime::mutation_response(
        &published.value.cursor,
        Json(ViewSwitchResponse { session_id, view }),
    )
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
    crate::server::reload::merge_runtime_fields(cached, &mut persisted);
    persisted
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
    let namespace = state.profile_namespace.clone().read_owned().await;
    // Serializes the transition with disable and with the deferred spawn.
    let inst_lock = state.instance_lock(&id).await;
    let _transition_guard = inst_lock.lock().await;
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
        drop(namespace);
        return view_response(&state, id, View::Structured).await;
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
        commit_structured_view(&state, &mut instance, selected_binding.as_ref(), namespace).await
    {
        return resp;
    }
    spawn_enabled_worker(state.clone(), instance, agent_name, selected_conversation);
    view_response(&state, id, View::Structured).await
}

/// Commit the structured view and publish its authoritative status once.
async fn commit_structured_view(
    state: &Arc<AppState>,
    instance: &mut Instance,
    selected_binding: Option<&crate::session::ConversationBinding>,
    namespace: tokio::sync::OwnedRwLockReadGuard<()>,
) -> Result<(), Response> {
    let inst_for_transition = instance.clone();
    let profile = instance.source_profile.clone();
    let commit_state = state.clone();
    let binding_for_transition = selected_binding.cloned();
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let _namespace = namespace;
        use crate::session::SessionStore;
        let storage = crate::server::session_store::NativeSessionStore::open(
            commit_state, &profile, Some(inst_for_transition.id.clone()),
        )?;
        let _lifecycle_lock = storage.storage()
            .acquire_instance_lifecycle_lock(&inst_for_transition.id)
            .map_err(|error| {
                anyhow::anyhow!("failed to acquire terminal-to-ACP lifecycle lock: {error}")
            })?;
        if let Err(e) = inst_for_transition.kill_locked() {
            tracing::warn!(target: "acp.switch", session = %inst_for_transition.id, "kill tmux failed: {e}");
        }
        if let Err(error) = inst_for_transition.kill_ancillary_tmux_sessions_locked() {
            tracing::warn!(target: "acp.switch", session = %inst_for_transition.id, "kill ancillary tmux sessions failed: {error}");
        }
        // Still the terminal row here, so this stops the pane's scope; the
        // worker spawns after the transition, under a fresh unit name.
        inst_for_transition.release_scope()?;
        (&storage as &dyn crate::session::SessionStore).update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == inst_for_transition.id)
            else {
                anyhow::bail!("session disappeared during terminal-to-ACP transition");
            };
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
            slot.acp_load_session_capable = None;
            Ok(slot.lifecycle_generation)
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
    if let Some(committed) = find_instance(state, &id).await {
        if lifecycle_generation >= instance.lifecycle_generation {
            *instance = committed;
        }
    }
    state.runtime.publish(state).await.map_err(|error| {
        tracing::error!(target: "acp.switch", session = %id, %error, "view publication failed");
        internal_error("failed to publish structured view")
    })?;
    Ok(())
}

/// Spawn the worker in the background so the response does not wait on a
/// container pull. Failures surface as a startup-error banner.
fn spawn_enabled_worker(
    state: Arc<AppState>,

    expected: Instance,
    agent_name: String,
    selected_conversation: Option<(String, crate::session::ExecutionBinding)>,
) {
    let session_id = expected.id.clone();
    let work = state.runtime.work.clone();
    work.spawn("server.acp_enable", async move {
        let exclusion =
            match crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &session_id, false)
                .await
            {
                Ok(exclusion) => exclusion,
                Err(_) => return,
            };
        let supervisor = &state.acp_supervisor;
        let (request, reservation) = match deferred_enable_request(
            &state,
            &expected,
            agent_name.clone(),
            selected_conversation,
            exclusion,
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
        if let Err(e) = supervisor.spawn_inner(request, reservation).await {
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
    state: &Arc<AppState>,
    expected: &Instance,
    agent_name: String,
    selected_conversation: Option<(String, crate::session::ExecutionBinding)>,
    exclusion: crate::acp::sandbox::LaunchExclusion,
) -> Option<Result<(SpawnRequest, crate::acp::supervisor::ResumeReservation), String>> {
    let session_id = expected.id.as_str();
    let instance = find_instance(state, session_id).await.filter(|row| {
        row.is_structured()
            && row.launch_is_finalized()
            && row.lifecycle_generation == expected.lifecycle_generation
            && row.source_profile == expected.source_profile
            && row.ensure_startable().is_ok()
    })?;
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
    let (native, exclusion) = match launch_store_for(state, &instance, exclusion).await {
        Ok(store) => store,
        Err(_) => return Some(Err("session source unavailable".to_owned())),
    };
    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!(target: "acp.switch", session = %session_id, "container ensure failed: {e}");
            return Some(Err(format!("container start failed: {e}")));
        }
    };
    let reservation = match state.acp_supervisor.reserve_spawn(session_id).await {
        Ok(reservation) => reservation,
        Err(error) => return Some(Err(error.to_string())),
    };
    drop(exclusion);
    Some(Ok((
        SpawnRequest {
            launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
                store: native,
                generation: expected.lifecycle_generation,
                namespace: Some(state.profile_namespace.clone()),
            }),
            stored_acp_session_id: seed.stored_acp_session_id,
            seed_history_replay: seed.seed_history_replay,
            sandbox_continuation: if seed.import_terminal {
                crate::acp::supervisor::SandboxContinuation::ImportTerminal
            } else {
                crate::acp::supervisor::SandboxContinuation::Persisted
            },
            claude_store_pin,
            ..spawn_request_for(&instance, agent_name, sandbox_info)
        },
        reservation,
    )))
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
    let namespace = state.profile_namespace.clone().read_owned().await;
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
    let mut already_terminal = false;

    if !instance.is_structured() {
        // A reload may have cached a pre-enable terminal snapshot; trust the
        // durable row before answering idempotently.
        match crate::server::api::load_persisted_instance(&state, &profile, &id).await {
            Ok(Some(durable)) if durable.is_structured() => {
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
            Ok(Some(durable)) => {
                already_terminal = true;
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
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
    if keep_context && !already_terminal {
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
    } else if !already_terminal {
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

    // Keep the transition guard until the matching in-flight launch has retired.
    let shutdown_result = if keep_context {
        state
            .acp_supervisor
            .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
            .await
    } else {
        state
            .acp_supervisor
            .shutdown_and_delete_and_wait(&id, std::time::Duration::from_secs(5))
            .await
    };
    if let Err(error) = shutdown_result {
        return supervisor_error_response("terminal handoff", &error);
    }
    if let Err(error) = state.acp_supervisor.forget_session(&id) {
        return supervisor_error_response("terminal handoff", &error);
    }
    // A tmux pane reprints a kept conversation only after its runner is gone.
    state.acp_event_store.delete_session(&id);

    // Hooks run without preparation guards; retirement is already proven.
    drop(namespace);
    drop(_guard);
    drop(_submission);
    if !already_terminal {
        let launch_state = state.clone();
        let launch = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let native = crate::server::session_store::NativeSessionStore::open(
                launch_state,
                &profile,
                Some(instance.id.clone()),
            )?;
            instance.start_in(&native)?;
            let _title = crate::session::acquire_session_title_lock(&instance.id)?;
            let _lifecycle = native
                .storage()
                .acquire_instance_lifecycle_lock(&instance.id)?;
            native.adopt_runtime_fields(&instance)?;
            native.refresh_pane_observations(&instance)?;
            Ok(())
        })
        .await;
        match launch {
            Ok(Ok(())) => {}
            // An archived or trashed session switches views without starting.
            Ok(Err(error))
                if error
                    .downcast_ref::<crate::session::StartBlocked>()
                    .is_some() => {}
            Ok(Err(error)) => {
                return (
                    StatusCode::CONFLICT,
                    format!("terminal handoff launch failed: {error}"),
                )
                    .into_response();
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("terminal handoff launch failed: {error}"),
                )
                    .into_response();
            }
        }
    }
    view_response(&state, id, View::Terminal).await
}

/// Persist the terminal handoff with compare-and-swap guards on both cache and disk.
async fn persist_terminal_view(
    state: &Arc<AppState>,
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
    let commit_state = state.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::server::session_store::NativeSessionStore::open(
            commit_state,
            &profile_for_save,
            Some(snapshot.id.clone()),
        )?;
        let _lifecycle = storage
            .storage()
            .acquire_instance_lifecycle_lock(&snapshot.id)?;
        (&storage as &dyn crate::session::SessionStore).update(|all, _groups| {
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
            slot.acp_load_session_capable = None;
            slot.acp_session_id = snapshot.acp_session_id.clone();
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pending_handoff(
        row: &Instance,
        pid: u32,
        control: Arc<crate::acp::runner_lifecycle::test_support::FakeProcessControl>,
    ) -> (
        Arc<AppState>,
        crate::acp::supervisor::test_support::Gate,
        tokio::task::JoinHandle<Result<(), SupervisorError>>,
        crate::acp::runner_lifecycle::Lease,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        use crate::acp::supervisor::test_support::{recording_gated_launcher, spawn_request, Gate};
        crate::server::test_support::seed_instances_on_disk_for_test(
            &row.source_profile,
            vec![row.clone()],
        );
        let gate = Gate::default();
        let (launcher, deleted) =
            recording_gated_launcher(&gate, pid, &row.id, "gated-native-session");
        let state = crate::server::test_support::build_test_app_state_impl(
            vec![row.clone()],
            Vec::new(),
            Vec::new(),
            None,
            false,
            |_| {},
            |supervisor| {
                supervisor
                    .with_process_control(control)
                    .with_launcher(launcher)
            },
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let reservation = state.acp_supervisor.reserve_spawn(&row.id).await.unwrap();
        let lease = reservation.lease().clone();
        let mut request = spawn_request(&row.id);
        request.source_profile = Some(row.source_profile.clone());
        let supervisor = state.acp_supervisor.clone();
        let spawner =
            tokio::spawn(async move { supervisor.spawn_inner(request, reservation).await });
        gate.entered.notified().await;
        (state, gate, spawner, lease, deleted)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disable_retires_a_pending_handshake_before_publishing_terminal() -> anyhow::Result<()>
    {
        use crate::acp::runner_lifecycle::{test_support::FakeProcessControl, ProcessControl};
        use crate::process::worker_registry;
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })?;
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().unwrap();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(pid);
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("pending terminal handoff", project.path().to_str().unwrap());
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.command = "sleep 60".into();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        let (state, gate, spawner, lease, deleted) =
            pending_handoff(&row, pid, control.clone()).await;
        let enabled = acp_enable(State(state.clone()), Path(row.id.clone()))
            .await
            .into_response();
        assert_eq!(enabled.status(), StatusCode::OK);
        assert!(enabled
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        let sampled = worker_registry::load(&row.id)?.unwrap();
        assert!(
            worker_registry::is_record_live(&sampled),
            "the pre-handoff repair sample is genuinely live"
        );
        let peer = Instance::new(
            "peer committed during startup",
            project.path().to_str().unwrap(),
        );
        let storage = crate::session::Storage::new_unwatched(&row.source_profile)?;
        storage.update(|rows, _| {
            rows.push(peer.clone());
            Ok(())
        })?;
        let disabling = acp_disable(State(state.clone()), Path(row.id.clone()));
        tokio::pin!(disabling);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    response = &mut disabling => panic!("disable answered before retirement: {}", response.into_response().status()),
                    _ = tokio::task::yield_now() => {
                        if state.acp_supervisor.test_cancel_requested(&lease) { break; }
                    }
                }
            }
        }).await?;
        let transition = state.instance_lock(&row.id).await;
        assert!(
            transition.try_lock().is_err(),
            "handoff still owns the view exclusion"
        );
        let during_epoch = state.mutation_epoch.load(Ordering::SeqCst);
        let during_metadata = state.canonical_metadata.read().await.clone();
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            storage.load()?,
            vec![(sampled.clone(), "gated-native-session".into())],
            crate::server::state::StatusSource::DiskOnly,
            during_epoch,
            during_metadata,
            Default::default(),
        )
        .await;
        assert!(
            !state
                .instances
                .read()
                .await
                .iter()
                .find(|session| session.id == row.id)
                .unwrap()
                .is_structured(),
            "reload respects the actual pending handoff exclusion"
        );
        assert!(!storage
            .load()?
            .iter()
            .find(|session| session.id == row.id)
            .unwrap()
            .is_structured());
        gate.open.notify_one();
        let response = disabling.await.into_response();
        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
            anyhow::bail!(
                "disable refused with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        let epoch = response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER]
            .to_str()?
            .to_owned();
        let revision = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
            .to_str()?
            .parse::<u64>()?;
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(body["view"], "terminal");
        assert!(
            spawner.await?.is_err(),
            "the pending structured launch was refused"
        );
        assert!(
            deleted.load(Ordering::SeqCst),
            "permanent cancellation deletes the late native transcript"
        );
        assert!(control.signals().contains(&(pid, "TERM")));
        assert!(!control.is_alive(pid));
        assert!(worker_registry::load(&row.id)?.is_none());
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(epoch, snapshot.value.cursor.epoch);
        assert!(snapshot.value.cursor.revision >= revision);
        let terminal = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert!(terminal.lifecycle_generation > row.lifecycle_generation);
        let published_start = (
            terminal.lifecycle_generation,
            Status::from_api_str(&terminal.status),
        );
        assert!(terminal.lifecycle_reservation.is_none());
        assert_eq!(
            terminal.agent_pane.state,
            crate::session::PanePresence::Alive
        );
        assert_eq!(
            terminal.agent_pane.tmux_session.as_deref(),
            Some(crate::tmux::Session::generate_name(&row.id, &row.title).as_str())
        );
        assert!(
            snapshot
                .value
                .contents
                .sessions
                .iter()
                .any(|session| session.id == peer.id),
            "the complete committed profile was adopted"
        );
        let persisted = storage.load()?;
        let terminal = persisted
            .iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert!(terminal.lifecycle_generation > row.lifecycle_generation);
        let terminal_generation = terminal.lifecycle_generation;
        assert_eq!(
            published_start,
            (terminal.lifecycle_generation, Some(terminal.status)),
            "the receipt publishes the committed native start status and generation"
        );
        assert!(terminal.lifecycle_reservation.is_none());
        let epoch_before_reload = state.mutation_epoch.load(Ordering::SeqCst);
        let metadata = state.canonical_metadata.read().await.clone();
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            persisted,
            vec![(sampled, "gated-native-session".into())],
            crate::server::state::StatusSource::DiskOnly,
            epoch_before_reload,
            metadata,
            Default::default(),
        )
        .await;
        assert!(
            !state
                .instances
                .read()
                .await
                .iter()
                .find(|session| session.id == row.id)
                .unwrap()
                .is_structured(),
            "a stale live sample cannot resurrect the retired structured runner"
        );
        assert!(!storage
            .load()?
            .iter()
            .find(|session| session.id == row.id)
            .unwrap()
            .is_structured());
        let idempotent = acp_disable(State(state.clone()), Path(row.id.clone()))
            .await
            .into_response();
        assert_eq!(idempotent.status(), StatusCode::OK);
        assert!(idempotent
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        assert_eq!(
            storage
                .load()?
                .iter()
                .find(|session| session.id == row.id)
                .unwrap()
                .lifecycle_generation,
            terminal_generation,
            "idempotent disable does not launch again"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disable_refuses_success_and_retains_unproven_retirement() -> anyhow::Result<()> {
        use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
        use crate::process::worker_registry;
        use std::time::Duration;
        let _home = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("unproven terminal handoff", "/tmp");
        row.view = View::Structured;
        row.source_profile = "default".into();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(4747);
        let (state, gate, spawner, lease, _) = pending_handoff(&row, 4747, control.clone()).await;
        let disabling = acp_disable(State(state.clone()), Path(row.id.clone()));
        tokio::pin!(disabling);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    response = &mut disabling => panic!("disable answered before cancellation: {}", response.into_response().status()),
                    _ = tokio::task::yield_now() => {
                        if state.acp_supervisor.test_cancel_requested(&lease) { break; }
                    }
                }
            }
        }).await?;
        gate.open.notify_one();
        let response = disabling.await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        assert!(state.acp_supervisor.is_owned(&row.id).await);
        assert!(matches!(
            state.acp_supervisor.forget_session(&row.id),
            Err(SupervisorError::TeardownPending(_))
        ));
        assert!(worker_registry::load(&row.id)?.is_some());
        assert!(control.signals().contains(&(4747, "KILL")));
        assert!(spawner.await?.is_err());
        control.exit(4747);
        state.acp_supervisor.retry_pending_teardowns().await;
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
        assert!(worker_registry::load(&row.id)?.is_none());
        Ok(())
    }

    /// An enable queued behind the transition lock must spawn on a provider
    /// switch that committed while it waited, not on the row it was queued with.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_queued_enable_spawns_on_the_live_provider() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("claude", "/tmp/aoe-queued-enable");
        inst.source_profile = "default".into();
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
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![inst.clone()],
            launcher,
        );

        let inst_lock = state.instance_lock(&id).await;
        let guard = inst_lock.lock().await;
        spawn_enabled_worker(state.clone(), inst.clone(), "claude".to_string(), None);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let (store, ()) = crate::server::session_store::NativeSessionStore::open_for_launch(
            state.clone(),
            &inst,
            (),
        )
        .await
        .unwrap();
        tokio::task::spawn_blocking(move || {
            use crate::session::SessionStore;
            store.commit(&mut |rows, _| {
                rows[0].agent_provider = Some("vertex".to_owned());
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
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
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut rx = state.status_tx.subscribe();
        let mut working = inst.clone();

        let namespace = state.profile_namespace.clone().read_owned().await;
        assert!(
            commit_structured_view(&state, &mut working, None, namespace)
                .await
                .is_ok()
        );

        let change = rx.try_recv().expect("the live slot's move is published");
        assert_eq!(change.instance_id, inst.id);
        assert_eq!((change.old, change.new), (Status::Running, Status::Idle));
        assert_eq!(change.effective_profile, "default");
        assert!(
            rx.try_recv().is_err(),
            "the transition is published only once"
        );
        let snapshot = state.runtime.snapshot(&state).await.unwrap();
        let row = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|row| row.id == inst.id)
            .unwrap();
        assert_eq!(Status::from_api_str(&row.status), Some(Status::Idle));
        assert_eq!(row.view, View::Structured);
        let persisted = crate::session::Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            (persisted[0].status, persisted[0].view),
            (Status::Idle, View::Structured)
        );
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

//! Switching a session between the terminal (tmux) and structured views.

use serde::Serialize;

use crate::server::api::{find_instance, instance_exists};
use crate::session::{Held, Instance, ResumeIntent, SessionStore, Status, Unserved, View};

use super::*;

#[derive(Debug, Serialize)]
pub struct ViewSwitchResponse {
    pub session_id: String,
    pub view: View,
}

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
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
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
    tracing::info!(
        target: "acp.switch",
        session = %id,
        to = "structured",
        agent = %agent_name,
        "view switched"
    );
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
        let current = storage.load()?.into_iter()
            .find(|row| row.id == inst_for_transition.id)
            .ok_or(crate::session::SessionGone)?;
        current.ensure_startable()?;
        anyhow::ensure!(
            current.lifecycle_generation == inst_for_transition.lifecycle_generation
                && current.view == inst_for_transition.view
                && current.terminal_handoff.is_none(),
            crate::session::LifecycleReservationError::Superseded
        );
        let generation = current.lifecycle_generation.checked_add(1)
            .ok_or(crate::session::LifecycleReservationError::GenerationOverflow)?;
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
            anyhow::ensure!(
                slot.lifecycle_generation == inst_for_transition.lifecycle_generation
                    && slot.view == inst_for_transition.view
                    && slot.terminal_handoff.is_none(),
                crate::session::LifecycleReservationError::Superseded
            );
            slot.ensure_startable()?;
            slot.view = View::Structured;
            slot.resume_intent = ResumeIntent::Default;
            if let Some(binding) = &binding_for_transition {
                slot.agent_session_id = Some(binding.session_id.clone());
                slot.agent_session_binding = Some(binding.clone());
            }
            // Structured status is event-driven and the tmux poller skips
            // structured rows, so a Running/Waiting status would never settle.
            slot.status = Status::Idle;
            slot.idle_dormant_since = None;
            slot.idle_entered_at = None;
            slot.last_error = None;
            slot.lifecycle_generation = generation;
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
                tracing::warn!(target: "acp.switch", session = %session_id, %message,
                    "deferred structured launch failed");
                publish_startup_error_if_current(&state, &session_id, &expected.source_profile,
                    expected.lifecycle_generation, message).await;
                return;
            }
        };
        if let Err(e) = supervisor
            .spawn_inner(
                request, reservation)
            .await
        {
            let message = structured_spawn_error_message(&e, &agent_name);
            tracing::warn!(target: "acp.switch", session = %session_id, "spawn after enable: {message}");
            publish_startup_error_if_current(&state, &session_id, &expected.source_profile,
                expected.lifecycle_generation, message).await;
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
            && row.terminal_handoff.is_none()
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
    let reservation = match state
        .acp_supervisor
        .reserve_spawn(
            session_id,
            if seed.stored_acp_session_id.is_some() {
                crate::acp::supervisor::ResumeKind::Spawn
            } else {
                crate::acp::supervisor::ResumeKind::SpawnFirst
            },
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await
    {
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
            prior_conversation: instance.acp_session_id.is_some()
                || instance.agent_session_id.is_some()
                || instance.pi_session_path.is_some()
                || selected_conversation.is_some(),
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

type HandoffKey = crate::session::TerminalHandoffKey;
type HandoffIntent = crate::session::TerminalHandoffIntent;
type HandoffStage = crate::session::TerminalHandoffStage;
type HandoffContext = crate::session::TerminalHandoffContext;
type NativeHandoffStore = crate::server::session_store::NativeSessionStore;

pub(crate) enum HandoffRecovery {
    Maintenance,
    Startup,
}

struct HandoffExclusion {
    _namespace: tokio::sync::OwnedRwLockReadGuard<()>,
    _submission: tokio::sync::OwnedMutexGuard<()>,
    _instance: tokio::sync::OwnedMutexGuard<()>,
}

impl HandoffExclusion {
    async fn acquire(state: &Arc<AppState>, id: &str) -> anyhow::Result<Self> {
        let namespace = state.profile_namespace.clone().read_owned().await;
        let submission = state
            .session_service
            .prompt_submission_for_session(id)
            .await
            .ok_or(crate::session::SessionGone)?;
        let instance = state.instance_lock(id).await.lock_owned().await;
        Ok(Self {
            _namespace: namespace,
            _submission: submission,
            _instance: instance,
        })
    }
}

struct TerminalHandoff {
    native: Option<NativeHandoffStore>,
    session_id: String,
    key: Option<HandoffKey>,
    driver: Arc<crate::session::TerminalHandoffDriver>,
}

impl TerminalHandoff {
    fn new(
        native: NativeHandoffStore,
        session_id: String,
        driver: Arc<crate::session::TerminalHandoffDriver>,
    ) -> Self {
        Self {
            native: Some(native),
            session_id,
            key: None,
            driver,
        }
    }

    fn store(&self) -> &NativeHandoffStore {
        self.native.as_ref().expect("owned handoff store")
    }

    fn update_owned(
        &mut self,
        mutate: impl FnOnce(&mut Vec<Instance>) -> anyhow::Result<HandoffKey>,
    ) -> anyhow::Result<HandoffKey> {
        let mut candidate = None;
        let result = (self.store() as &dyn SessionStore).update(|rows, _| {
            let key = mutate(rows)?;
            candidate = Some(key);
            Ok(())
        });
        let landed = match &result {
            Ok(()) => true,
            Err(error) if error.is::<crate::session::SessionCommitApplied>() => true,
            Err(_) => candidate.is_some_and(|key| self.owns_on_disk(key)),
        };
        if landed {
            self.key = candidate;
        }
        result?;
        candidate.ok_or_else(|| anyhow::anyhow!("handoff mutation was not invoked"))
    }

    fn owns_on_disk(&self, key: HandoffKey) -> bool {
        if crate::migrations::ensure_schema_supported().is_err() {
            return false;
        }
        let storage = self.store().storage();
        let Ok(identity) = storage.bound_profile_identity() else {
            return false;
        };
        storage.load_complete_with_groups().is_ok_and(|(rows, _)| {
            rows.iter().any(|row| {
                row.id == self.session_id
                    && row.lifecycle_reservation_is_owned(
                        crate::session::LifecycleOperation::Launch,
                        key.generation,
                    )
                    && row.terminal_handoff.as_ref().is_some_and(|intent| {
                        intent.id == key.id
                            && intent.generation == key.generation
                            && intent.profile == storage.profile()
                            && intent.profile_identity == identity
                    })
            })
        })
    }
}

impl Drop for TerminalHandoff {
    fn drop(&mut self) {
        if let (Some(native), Some(key)) = (self.native.take(), self.key) {
            SessionStore::defer_lifecycle_release(
                native,
                self.session_id.clone(),
                crate::session::LifecycleOperation::Launch,
                key.generation,
                None,
            );
        }
    }
}

fn handoff_unavailable(error: &anyhow::Error) -> Response {
    tracing::warn!(target: "acp.switch", %error, "terminal handoff remains pending");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "terminal handoff remains pending; retry after runtime recovery",
    )
        .into_response()
}

fn matching_handoff<'a>(
    row: &'a mut Instance,
    profile: &str,
    profile_identity: (u64, u64),
    key: HandoffKey,
) -> anyhow::Result<&'a mut HandoffIntent> {
    anyhow::ensure!(
        row.lifecycle_generation == key.generation,
        crate::session::LifecycleReservationError::Superseded
    );
    let intent = row
        .terminal_handoff
        .as_mut()
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    anyhow::ensure!(
        intent.id == key.id
            && intent.generation == key.generation
            && intent.target_generation <= key.generation
            && intent.profile == profile
            && intent.profile_identity == profile_identity,
        crate::session::LifecycleReservationError::Superseded
    );
    Ok(intent)
}

async fn load_handoff_row(
    state: &Arc<AppState>,
    profile: &str,
    id: &str,
) -> anyhow::Result<(NativeHandoffStore, Instance)> {
    let load_state = state.clone();
    let load_profile = profile.to_owned();
    let load_id = id.to_owned();
    let (native, mut row) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut native = NativeHandoffStore::open(load_state, &load_profile, None)?;
        let mut row = native
            .load()?
            .into_iter()
            .find(|row| row.id == load_id)
            .ok_or(crate::session::SessionGone)?;
        row.source_profile = load_profile;
        if row
            .terminal_handoff
            .as_ref()
            .is_some_and(|intent| intent.stage != HandoffStage::RetirementPending)
        {
            native.set_status_id(load_id);
        }
        Ok((native, row))
    })
    .await??;
    if let Some(prior) = find_instance(state, id).await.filter(|prior| {
        prior.source_profile == profile && prior.lifecycle_generation <= row.lifecycle_generation
    }) {
        row.capture_poller.adopt_reload(
            &prior.capture_poller,
            prior.held(),
            Held::new(&row.tool, row.active_execution.as_ref()),
            Unserved::Leave,
        );
    }
    Ok((native, row))
}

fn handoff_key(row: &Instance) -> anyhow::Result<HandoffKey> {
    let intent = row
        .terminal_handoff
        .as_ref()
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    anyhow::ensure!(
        intent.generation == row.lifecycle_generation,
        crate::session::LifecycleReservationError::Superseded
    );
    Ok(HandoffKey {
        id: intent.id,
        generation: intent.generation,
    })
}

fn claim_handoff_in(
    mut owner: TerminalHandoff,
    expected: HandoffKey,
) -> anyhow::Result<TerminalHandoff> {
    use crate::session::{LifecycleOperation, LifecycleReservation, LifecycleReservationError};
    let profile = owner.store().storage().profile().to_owned();
    let identity = owner.store().storage().bound_profile_identity()?;
    let id = owner.session_id.clone();
    anyhow::ensure!(
        owner.driver.session_id() == id.as_str(),
        LifecycleReservationError::Superseded
    );
    let _title = crate::session::acquire_session_title_lock(&id)?;
    let _lifecycle = owner
        .store()
        .storage()
        .acquire_instance_lifecycle_lock(&id)?;
    owner.update_owned(|rows| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        matching_handoff(row, &profile, identity, expected)?;
        let generation = row
            .lifecycle_generation
            .checked_add(1)
            .ok_or(LifecycleReservationError::GenerationOverflow)?;
        row.lifecycle_generation = generation;
        row.lifecycle_reservation = Some(LifecycleReservation {
            op: LifecycleOperation::Launch,
            generation,
            at: chrono::Utc::now(),
        });
        row.terminal_handoff
            .as_mut()
            .expect("validated handoff")
            .generation = generation;
        Ok(HandoffKey {
            id: expected.id,
            generation,
        })
    })?;
    Ok(owner)
}

fn reserve_handoff_in(
    native: NativeHandoffStore,
    expected: &Instance,
    context: HandoffContext,
    source_event_seq: u64,
    state: Arc<AppState>,
    driver: Arc<crate::session::TerminalHandoffDriver>,
) -> anyhow::Result<TerminalHandoff> {
    use crate::session::{LifecycleOperation, LifecycleReservationError};
    anyhow::ensure!(
        driver.session_id() == expected.id.as_str(),
        LifecycleReservationError::Superseded
    );
    let mut owner = TerminalHandoff::new(native, expected.id.clone(), driver);
    let profile = owner.store().storage().profile().to_owned();
    let identity = owner.store().storage().bound_profile_identity()?;
    let _title = crate::session::acquire_session_title_lock(&expected.id)?;
    let _lifecycle = owner
        .store()
        .storage()
        .acquire_instance_lifecycle_lock(&expected.id)?;
    let (target, may_fence_unborn) = state.acp_supervisor.capture_handoff_retirement(
        &expected.id,
        &profile,
        identity,
        expected.lifecycle_generation,
    )?;
    let mut runner = crate::session::purge_owners::capture_runner(&expected.id);
    runner.bind_retirement(&target)?;
    if let crate::session::purge_owners::RunnerCapture::Captured {
        generation,
        birth_fingerprint,
        prime,
        ..
    } = &runner
    {
        anyhow::ensure!(*generation != 0 && birth_fingerprint.is_some()
            && !matches!(prime, crate::acp::runner_lifecycle::PrimeOwnership::Unresolved),
            "original physical producer lacks complete birth/generation/Prime authority; no handoff reserved");
    }
    if target
        .lease
        .as_ref()
        .is_some_and(|lease| lease.was_cancelled_before_birth())
    {
        runner = runner.completed(&target)?;
    }
    let unborn_producer = match &runner {
        crate::session::purge_owners::RunnerCapture::Uncaptured {
            producer_epoch: Some(epoch),
            producer_id: Some(id),
            scopes,
            ..
        } => {
            anyhow::ensure!(
                may_fence_unborn && scopes.is_empty(),
                "original unborn producer has no bounded native birth fence"
            );
            Some((*epoch, *id))
        }
        _ => None,
    };
    let intent_id = uuid::Uuid::new_v4();
    owner.update_owned(|rows| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == expected.id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            row.terminal_handoff.is_none()
                && row.view == View::Structured
                && row.lifecycle_generation == expected.lifecycle_generation
                && row.acp_session_id == expected.acp_session_id
                && expected.conversation_state().matches(row),
            LifecycleReservationError::Superseded
        );
        let source_generation = row.lifecycle_generation;
        let generation = row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Launch,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        let unborn_fence = unborn_producer.map(|(producer_epoch, producer_id)| {
            crate::session::TerminalHandoffUnbornFence {
                producer_epoch,
                producer_id,
                source_generation,
                fence_generation: generation,
            }
        });
        row.terminal_handoff = Some(Box::new(HandoffIntent {
            id: intent_id,
            profile,
            profile_identity: identity,
            generation,
            target_generation: generation,
            stage: HandoffStage::RetirementPending,
            context,
            source_acp_session_id: row.acp_session_id.clone(),
            source_event_seq,
            final_event_seq: None,
            source_conversation: row.conversation_state(),
            runner,
            unborn_fence,
            terminal: None,
            launch_required: row.ensure_startable().is_ok(),
            launch_started: false,
            launch: None,
        }));
        Ok(HandoffKey {
            id: intent_id,
            generation,
        })
    })?;
    Ok(owner)
}

fn record_retired_handoff_in(
    native: &crate::server::session_store::NativeSessionStore,
    session_id: &str,
    key: HandoffKey,
    completed_runner: crate::session::purge_owners::RunnerCapture,
    final_event_seq: u64,
    final_context_event: Option<crate::acp::Event>,
) -> anyhow::Result<()> {
    use crate::session::{
        LifecycleOperation, LifecycleReservationError, TerminalHandoffContext, TerminalHandoffStage,
    };
    let profile = native.storage().profile();
    let profile_identity = native.storage().bound_profile_identity()?;
    let mut target_error = None;
    (native as &dyn SessionStore).update(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == session_id)
            .ok_or(crate::session::SessionGone)?;
        let intent = matching_handoff(row, profile, profile_identity, key)?.clone();
        anyhow::ensure!(
            intent.stage == TerminalHandoffStage::RetirementPending
                && row.view == View::Structured
                && row.tool == intent.source_conversation.tool
                && row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, key.generation),
            LifecycleReservationError::Superseded
        );
        if let Some(event) = final_context_event.as_ref() {
            let change = crate::server::acp_events::derive_acp_session_change(event);
            crate::server::acp_events::apply_acp_session_change(row, session_id, change.as_ref());
        }
        let terminal = match &intent.context {
            TerminalHandoffContext::Keep { native_store } => {
                match row.terminal_keep_context_state(Some(native_store)) {
                    Ok(terminal) => Some(terminal),
                    Err(error) => {
                        target_error = Some(error);
                        None
                    }
                }
            }
            TerminalHandoffContext::Discard => {
                if row.terminal_rebind_pending {
                    let tool = row
                        .agent_name
                        .as_deref()
                        .and_then(crate::agents::terminal_tool_for_acp_agent)
                        .ok_or(LifecycleReservationError::Superseded)?;
                    row.rebind_terminal_tool_fresh(tool);
                    let config =
                        crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                            &row.source_profile,
                            std::path::Path::new(&row.project_path),
                        );
                    let yolo = row.yolo_mode;
                    crate::session::builder::apply_agent_launch_config(
                        row,
                        &config.session,
                        "",
                        "",
                        Some(yolo),
                    );
                }
                Some(row.conversation_state())
            }
        };
        intent.ensure_original_stopped(session_id)?;
        row.status = Status::Stopped;
        let source = row.conversation_state();
        let source_acp_session_id = row.acp_session_id.clone();
        let intent = row.terminal_handoff.as_mut().expect("validated handoff");
        intent.source_conversation = source;
        intent.source_acp_session_id = source_acp_session_id;
        intent.runner = completed_runner;
        intent.terminal = terminal;
        intent.final_event_seq = Some(final_event_seq);
        intent.stage = TerminalHandoffStage::TerminalCommitOwed;
        Ok(())
    })?;
    if let Some(error) = target_error {
        return Err(
            error.context("original retirement is durable, but its native target needs recovery")
        );
    }
    Ok(())
}

fn commit_terminal_in(
    native: &crate::server::session_store::NativeSessionStore,
    session_id: &str,
    key: HandoffKey,
) -> anyhow::Result<()> {
    use crate::session::{LifecycleOperation, LifecycleReservationError, TerminalHandoffStage};
    let profile = native.storage().profile();
    let profile_identity = native.storage().bound_profile_identity()?;
    (native as &dyn SessionStore).update(|rows, _| {
        let row = rows.iter_mut().find(|row| row.id == session_id)
            .ok_or(crate::session::SessionGone)?;
        let intent = matching_handoff(row, profile, profile_identity, key)?.clone();
        anyhow::ensure!(intent.stage == TerminalHandoffStage::TerminalCommitOwed
            && row.view == View::Structured
            && row.acp_session_id == intent.source_acp_session_id
            && intent.source_conversation.matches(row)
            && row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, key.generation),
            LifecycleReservationError::Superseded);
        let terminal = match intent.terminal {
            Some(terminal) => terminal,
            None => match &intent.context {
                crate::session::TerminalHandoffContext::Keep { native_store } =>
                    row.terminal_keep_context_state(Some(native_store)).map_err(|error|
                        error.context("kept target is unavailable; restore its native store or explicitly choose a target with aoe session set-session-id"))?,
                crate::session::TerminalHandoffContext::Discard => row.conversation_state(),
            },
        };
        row.adopt_conversation_state(terminal);
        row.view = View::Terminal;
        row.acp_session_id = None;
        row.import_pending = None;
        row.fork_pending = None;
        row.acp_load_session_capable = None;
        row.status = Status::Stopped;
        row.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, key.generation);
        let intent = row.terminal_handoff.as_mut().expect("validated handoff");
        intent.stage = if intent.launch_required {
            TerminalHandoffStage::TerminalLaunchOwed
        } else {
            TerminalHandoffStage::ProjectionCleanupOwed
        };
        Ok(())
    })
}

fn finish_handoff_in(
    native: &crate::server::session_store::NativeSessionStore,
    session_id: &str,
    key: HandoffKey,
) -> anyhow::Result<()> {
    use crate::session::{LifecycleOperation, LifecycleReservationError, TerminalHandoffStage};
    let profile = native.storage().profile();
    let profile_identity = native.storage().bound_profile_identity()?;
    (native as &dyn SessionStore).update(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            row.view == View::Terminal,
            LifecycleReservationError::Superseded
        );
        let intent = matching_handoff(row, profile, profile_identity, key)?;
        anyhow::ensure!(
            intent.stage == TerminalHandoffStage::ProjectionCleanupOwed,
            LifecycleReservationError::Superseded
        );
        row.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, key.generation);
        row.terminal_handoff = None;
        Ok(())
    })
}

pub async fn acp_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = read_only_block(&state) {
        return response;
    }
    if let Some(response) = cityhall_block(&state) {
        return response;
    }
    let work = state.runtime.work.clone();
    match work
        .run("server.acp_disable", async move {
            terminal_handoff_request(state, id).await
        })
        .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn terminal_handoff_request(state: Arc<AppState>, id: String) -> Response {
    if let Some(response) = read_only_block(&state) {
        return response;
    }
    if let Some(response) = cityhall_block(&state) {
        return response;
    }
    let driver = match crate::session::try_acquire_terminal_handoff_driver(&id) {
        Ok(Some(driver)) => Arc::new(driver),
        Ok(None) => {
            return handoff_unavailable(&anyhow::anyhow!("terminal handoff driver is active"))
        }
        Err(error) => return handoff_unavailable(&error),
    };
    let exclusion = match HandoffExclusion::acquire(&state, &id).await {
        Ok(exclusion) => exclusion,
        Err(error) if error.is::<crate::session::SessionGone>() => return session_not_found(),
        Err(error) => return handoff_unavailable(&error),
    };
    let Some(cached) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    let profile = cached.source_profile.clone();
    let source_event_seq = match state.acp_event_store.highest_seq_checked(&id) {
        Ok(seq) => seq,
        Err(error) => return handoff_unavailable(&error),
    };
    let (native, durable) = match load_handoff_row(&state, &profile, &id).await {
        Ok(loaded) => loaded,
        Err(error) if error.is::<crate::session::SessionGone>() => return session_not_found(),
        Err(error) => return handoff_unavailable(&error),
    };
    let Some(cached) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if cached.source_profile != profile {
        return (
            StatusCode::CONFLICT,
            "Session profile changed during terminal handoff; retry",
        )
            .into_response();
    }
    if durable.view == View::Terminal && durable.terminal_handoff.is_none() {
        drop(exclusion);
        return view_response(&state, id, View::Terminal).await;
    }
    let (key, owner) = if durable.terminal_handoff.is_some() {
        let key = match handoff_key(&durable) {
            Ok(key) => key,
            Err(error) => return handoff_unavailable(&error),
        };
        (key, None)
    } else {
        if cached.is_structured()
            && (cached.view != durable.view
                || cached.acp_session_id != durable.acp_session_id
                || !cached.conversation_state().matches(&durable))
        {
            return (
                StatusCode::CONFLICT,
                "ACP identity changed during terminal handoff; retry",
            )
                .into_response();
        }
        let instance = adopt_persisted_structured_instance(cached, durable, &profile);
        let agent = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
        let rebind = instance.terminal_rebind_pending;
        if rebind && crate::agents::terminal_tool_for_acp_agent(&agent).is_none() {
            return (
                StatusCode::CONFLICT,
                format!(
                    "{agent} has no terminal tool; switch to another agent before terminal view"
                ),
            )
                .into_response();
        }
        let keep = !rebind
            && crate::agents::acp_transcript_cli_resumable(&instance.tool, &agent)
            && instance.acp_session_id.is_some();
        let context = if keep {
            match state
                .acp_event_store
                .handoff_context_event(&id, 0, source_event_seq)
            {
                Ok(Some((_, crate::acp::Event::AcpSessionAssigned { acp_session_id })))
                    if instance.acp_session_id.as_deref() == Some(acp_session_id.as_str()) => {}
                Ok(Some(_)) => {
                    return handoff_unavailable(&anyhow::anyhow!(
                        "ACP context projection has not confirmed its durable final identity"
                    ))
                }
                Ok(None) => {}
                Err(error) => return handoff_unavailable(&error),
            }
            let worker = state
                .acp_supervisor
                .native_handoff_store(
                    &id,
                    instance.acp_session_id.as_deref().expect("keep-context ID"),
                )
                .await;
            let target = match instance.terminal_keep_context_state(worker.as_ref()) {
                Ok(target) => target,
                Err(error) => return (StatusCode::CONFLICT, format!("{error:#}")).into_response(),
            };
            let Some(native_store) = target.resume_binding.and_then(|binding| binding.execution)
            else {
                return (
                    StatusCode::CONFLICT,
                    "native conversation store is not proven",
                )
                    .into_response();
            };
            HandoffContext::Keep { native_store }
        } else {
            HandoffContext::Discard
        };
        let reserve_state = state.clone();
        let reserve_driver = driver.clone();
        let reserved = tokio::task::spawn_blocking(move || {
            reserve_handoff_in(
                native,
                &instance,
                context,
                source_event_seq,
                reserve_state,
                reserve_driver,
            )
        })
        .await;
        let owner = match reserved {
            Ok(Ok(owner)) => owner,
            Ok(Err(error)) if error.is::<crate::session::SessionCommitApplied>() => {
                return handoff_unavailable(&error)
            }
            Ok(Err(error)) if error.is::<crate::session::LifecycleReservationError>() => {
                return (StatusCode::CONFLICT, format!("{error:#}")).into_response()
            }
            Ok(Err(error)) => return handoff_unavailable(&error),
            Err(error) => return handoff_unavailable(&anyhow::Error::new(error)),
        };
        let key = owner.key.expect("landed reservation");
        (key, Some(owner))
    };
    match drive_terminal_handoff(
        state.clone(),
        HandoffTarget {
            id: id.clone(),
            profile,
            intent_id: key.id,
        },
        Some(exclusion),
        owner,
        driver,
        true,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(target: "acp.switch", session = %id, to = "terminal", "view switched");
            view_response(&state, id, View::Terminal).await
        }
        Err(error) => handoff_unavailable(&error),
    }
}

struct HandoffTarget {
    id: String,
    profile: String,
    intent_id: uuid::Uuid,
}

fn drive_terminal_handoff(
    state: Arc<AppState>,
    target: HandoffTarget,
    mut exclusion: Option<HandoffExclusion>,
    mut owner: Option<TerminalHandoff>,
    driver: Arc<crate::session::TerminalHandoffDriver>,
    allow_retry: bool,
) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
    // Keep staged handoff state off the tracked-task admission stack.
    Box::pin(async move {
        let HandoffTarget {
            id,
            profile,
            intent_id,
        } = target;
        anyhow::ensure!(
            driver.session_id() == id.as_str(),
            crate::session::LifecycleReservationError::Superseded
        );
        let mut claimed = owner.is_some();
        loop {
            if exclusion.is_none() {
                exclusion = Some(HandoffExclusion::acquire(&state, &id).await?);
            }
            let (native, mut row) = load_handoff_row(&state, &profile, &id).await?;
            if row.terminal_handoff.is_none() {
                anyhow::ensure!(
                    row.view == View::Terminal,
                    crate::session::LifecycleReservationError::Superseded
                );
                return Ok(());
            }
            let key = handoff_key(&row)?;
            let identity = native.storage().bound_profile_identity()?;
            let intent = matching_handoff(&mut row, &profile, identity, key)?.clone();
            anyhow::ensure!(
                intent.id == intent_id,
                crate::session::LifecycleReservationError::Superseded
            );
            match intent.stage {
                _ if !claimed => {
                    let unclaimed = TerminalHandoff::new(native, id.clone(), driver.clone());
                    owner = Some(
                        tokio::task::spawn_blocking(move || claim_handoff_in(unclaimed, key))
                            .await??,
                    );
                    claimed = true;
                }
                HandoffStage::RetirementPending => {
                    let mut held = owner.take().expect("claimed handoff");
                    anyhow::ensure!(
                        held.key == Some(key),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    let mut target = state.acp_supervisor.capture_retirement(&id)?;
                    intent.ensure_retirement_target(&id, &mut target)?;
                    let deadline = std::time::Duration::from_secs(5);
                    match intent.context {
                        HandoffContext::Keep { .. } => {
                            state
                                .acp_supervisor
                                .shutdown_captured_and_wait(&id, &target, deadline)
                                .await?
                        }
                        HandoffContext::Discard => {
                            state
                                .acp_supervisor
                                .shutdown_captured_and_delete_and_wait(&id, &target, deadline)
                                .await?
                        }
                    }
                    let completed = intent.completed_runner(&id, &target)?;
                    if intent.unborn_fence.is_some() && target.lease.is_none() {
                        intent.ensure_original_stopped(&id)?;
                        let already = state
                            .acp_event_store
                            .handoff_stopped_after(&id, intent.source_event_seq)?;
                        state
                            .acp_supervisor
                            .complete_unowned_fenced_handoff(&id, already)
                            .await?;
                    }
                    let final_seq = state.acp_event_store.highest_seq_checked(&id)?;
                    let final_event = state
                        .acp_event_store
                        .handoff_context_event(&id, intent.source_event_seq, final_seq)?
                        .map(|(_, event)| event);
                    held.native
                        .as_mut()
                        .expect("owned store")
                        .set_status_id(id.clone());
                    let save_id = id.clone();
                    owner = Some(
                        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                            let _title = crate::session::acquire_session_title_lock(&save_id)?;
                            let _lifecycle = held
                                .store()
                                .storage()
                                .acquire_instance_lifecycle_lock(&save_id)?;
                            record_retired_handoff_in(
                                held.store(),
                                &save_id,
                                key,
                                completed,
                                final_seq,
                                final_event,
                            )?;
                            Ok(held)
                        })
                        .await??,
                    );
                    state.acp_supervisor.acknowledge_retirement(&target)?;
                }
                HandoffStage::TerminalCommitOwed => {
                    let held = owner.take().expect("claimed handoff");
                    anyhow::ensure!(
                        held.key == Some(key),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    intent.ensure_original_stopped(&id)?;
                    let mut target = state.acp_supervisor.capture_retirement(&id)?;
                    intent.ensure_retirement_target(&id, &mut target)?;
                    state.acp_supervisor.acknowledge_retirement(&target)?;
                    let save_id = id.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let _title = crate::session::acquire_session_title_lock(&save_id)?;
                        let _lifecycle = held
                            .store()
                            .storage()
                            .acquire_instance_lifecycle_lock(&save_id)?;
                        commit_terminal_in(held.store(), &save_id, key)
                    })
                    .await??;
                }
                HandoffStage::TerminalLaunchOwed => {
                    drop(owner.take());
                    drop(exclusion.take());
                    let launch_state = state.clone();
                    let launch_id = id.clone();
                    let launch_driver = driver.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let mut held =
                            TerminalHandoff::new(native, launch_id.clone(), launch_driver);
                        let result = row.start_terminal_handoff_in(
                            held.store(),
                            key,
                            &held.driver,
                            allow_retry,
                        );
                        let attempt = HandoffKey {
                            id: key.id,
                            generation: row.lifecycle_generation,
                        };
                        if held.owns_on_disk(attempt) {
                            held.key = Some(attempt);
                        }
                        result?;
                        let _exclusion = tokio::runtime::Handle::current()
                            .block_on(HandoffExclusion::acquire(&launch_state, &launch_id))?;
                        let _title = crate::session::acquire_session_title_lock(&launch_id)?;
                        let _lifecycle = held
                            .store()
                            .storage()
                            .acquire_instance_lifecycle_lock(&launch_id)?;
                        held.store().adopt_runtime_fields(&row)?;
                        held.store().refresh_pane_observations(&row)?;
                        let identity = held.store().storage().bound_profile_identity()?;
                        let profile = held.store().storage().profile();
                        (held.store() as &dyn SessionStore).update(|rows, _| {
                            let row = rows
                                .iter_mut()
                                .find(|row| row.id == launch_id)
                                .ok_or(crate::session::SessionGone)?;
                            anyhow::ensure!(
                                row.view == View::Terminal,
                                crate::session::LifecycleReservationError::Superseded
                            );
                            let intent = matching_handoff(row, profile, identity, attempt)?;
                            anyhow::ensure!(
                                intent.stage == HandoffStage::TerminalLaunchOwed,
                                crate::session::LifecycleReservationError::Superseded
                            );
                            intent.stage = HandoffStage::ProjectionCleanupOwed;
                            Ok(())
                        })
                    })
                    .await??;
                }
                HandoffStage::ProjectionCleanupOwed => {
                    drop(owner.take());
                    let cleanup_state = state.clone();
                    let cleanup_id = id.clone();
                    let cleanup_driver = driver.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let _driver = cleanup_driver;
                        let _title = crate::session::acquire_session_title_lock(&cleanup_id)?;
                        let _lifecycle = native
                            .storage()
                            .acquire_instance_lifecycle_lock(&cleanup_id)?;
                        let mut current = native
                            .load()?
                            .into_iter()
                            .find(|row| row.id == cleanup_id)
                            .ok_or(crate::session::SessionGone)?;
                        let identity = native.storage().bound_profile_identity()?;
                        let recorded = matching_handoff(
                            &mut current,
                            native.storage().profile(),
                            identity,
                            key,
                        )?;
                        anyhow::ensure!(
                            recorded.stage == HandoffStage::ProjectionCleanupOwed,
                            crate::session::LifecycleReservationError::Superseded
                        );
                        recorded.ensure_original_stopped(&cleanup_id)?;
                        let mut target = cleanup_state
                            .acp_supervisor
                            .capture_retirement(&cleanup_id)?;
                        recorded.ensure_retirement_target(&cleanup_id, &mut target)?;
                        cleanup_state
                            .acp_supervisor
                            .acknowledge_retirement(&target)?;
                        cleanup_state.acp_supervisor.forget_session(&cleanup_id)?;
                        cleanup_state
                            .acp_event_store
                            .delete_session_checked(&cleanup_id)?;
                        cleanup_state.acp_control_cache.forget(&cleanup_id);
                        finish_handoff_in(&native, &cleanup_id, key)
                    })
                    .await??;
                    return Ok(());
                }
            }
        }
    })
}

pub(crate) async fn recover_pending_terminal_handoffs(
    state: &Arc<AppState>,
    mode: HandoffRecovery,
) {
    if state.read_only
        || state.cityhall_mode
        || *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy
    {
        return;
    }
    let startup = matches!(mode, HandoffRecovery::Startup);
    let pending: Vec<_> = state
        .instances
        .read()
        .await
        .iter()
        .filter_map(|row| {
            row.terminal_handoff
                .as_ref()
                .map(|intent| (row.id.clone(), row.source_profile.clone(), intent.id))
        })
        .collect();
    for (id, profile, intent_id) in pending {
        let driver = match crate::session::try_acquire_terminal_handoff_driver(&id) {
            Ok(Some(driver)) => Arc::new(driver),
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(target: "acp.switch", session = %id, %error, "handoff driver lock unavailable");
                continue;
            }
        };
        let recovery_state = state.clone();
        let work = state.runtime.work.clone();
        let log_id = id.clone();
        let result = work
            .run("server.terminal_handoff_recovery", async move {
                drive_terminal_handoff(
                    recovery_state,
                    HandoffTarget {
                        id,
                        profile,
                        intent_id,
                    },
                    None,
                    None,
                    driver,
                    startup,
                )
                .await
            })
            .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(target: "acp.switch", session = %log_id, %error,
                "persisted terminal handoff remains pending"),
            Err(_) => break,
        }
    }
}

pub(crate) async fn publish_startup_error_if_current(
    state: &Arc<AppState>,
    session_id: &str,
    profile: &str,
    generation: u64,
    message: String,
) -> bool {
    let _namespace = state.profile_namespace.read().await;
    let _instance = state.instance_lock(session_id).await.lock_owned().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return false;
    }
    let current = state.instances.read().await.iter().any(|row| {
        row.id == session_id
            && row.source_profile == profile
            && row.lifecycle_generation == generation
            && row.is_structured()
            && row.terminal_handoff.is_none()
    });
    if current {
        state
            .acp_supervisor
            .publish_startup_error(session_id, message);
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pending_handoff(
        row: &Instance,
        pid: u32,
        control: Arc<crate::acp::runner_lifecycle::test_support::FakeProcessControl>,
        original_birth: Option<crate::process::ProcessBirthIdentity>,
    ) -> (
        Arc<AppState>,
        crate::acp::supervisor::test_support::Gate,
        tokio::task::JoinHandle<Result<(), SupervisorError>>,
        crate::acp::runner_lifecycle::Lease,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        pending_handoff_with_event_store(row, pid, control, original_birth, None).await
    }

    async fn pending_handoff_with_event_store(
        row: &Instance,
        pid: u32,
        control: Arc<crate::acp::runner_lifecycle::test_support::FakeProcessControl>,
        original_birth: Option<crate::process::ProcessBirthIdentity>,
        event_store: Option<Arc<crate::acp::event_store::EventStore>>,
    ) -> (
        Arc<AppState>,
        crate::acp::supervisor::test_support::Gate,
        tokio::task::JoinHandle<Result<(), SupervisorError>>,
        crate::acp::runner_lifecycle::Lease,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        use crate::acp::supervisor::test_support::{recording_gated_launcher, Gate};
        let gate = Gate::default();
        let (launcher, deleted) =
            recording_gated_launcher(&gate, pid, &row.id, "gated-native-session", original_birth);
        let (state, spawner, lease) =
            handoff_producer_with_launcher(row, control, event_store, launcher).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("the admitted original producer must reach its handshake gate");
        (state, gate, spawner, lease, deleted)
    }

    async fn handoff_producer_with_launcher(
        row: &Instance,
        control: Arc<crate::acp::runner_lifecycle::test_support::FakeProcessControl>,
        event_store: Option<Arc<crate::acp::event_store::EventStore>>,
        launcher: crate::acp::supervisor::Launcher,
    ) -> (
        Arc<AppState>,
        tokio::task::JoinHandle<Result<(), SupervisorError>>,
        crate::acp::runner_lifecycle::Lease,
    ) {
        use crate::acp::supervisor::test_support::spawn_request;
        crate::server::test_support::seed_instances_on_disk_for_test(
            &row.source_profile,
            vec![row.clone()],
        );
        let state = crate::server::test_support::build_test_app_state_impl_with_event_store(
            vec![row.clone()],
            Vec::new(),
            Vec::new(),
            None,
            event_store,
            |_| {},
            |supervisor| {
                supervisor
                    .with_process_control(control)
                    .with_launcher(launcher)
            },
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let reservation = state
            .acp_supervisor
            .reserve_spawn(
                &row.id,
                crate::acp::supervisor::ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
            .unwrap();
        let lease = reservation.lease().clone();
        let mut request = spawn_request(&row.id);
        request.source_profile = Some(row.source_profile.clone());
        request.expected_lifecycle_generation = row.lifecycle_generation;
        request.launch_admission = Some(crate::acp::supervisor::LaunchAdmission {
            store: Arc::new(crate::session::Storage::open_unwatched(&row.source_profile).unwrap()),
            generation: row.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
        });
        lease.set_admission(request.launch_admission.clone());
        let supervisor = state.acp_supervisor.clone();
        let spawner =
            tokio::spawn(async move { supervisor.spawn_inner(request, reservation).await });
        (state, spawner, lease)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disable_refuses_an_agent_switch_to_an_agent_without_a_terminal_tool(
    ) -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("acp only", project.path().to_str().unwrap());
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.agent_name = Some("aoe-agent".into());
        row.terminal_rebind_pending = true;
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;

        let response = acp_disable(State(state.clone()), Path(row.id.clone()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let stored = crate::session::Storage::open_unwatched("default")?.load()?;
        let stored = stored.iter().find(|r| r.id == row.id).unwrap();
        assert_eq!(stored.view, View::Structured);
        assert_eq!(stored.tool, row.tool);
        assert!(stored.terminal_rebind_pending);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disable_after_agent_switch_rebinds_the_terminal_tool() -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true
        })?;
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("switched agent", project.path().to_str().unwrap());
        row.tool = "claude".into();
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.agent_name = Some("codex".into());
        row.terminal_rebind_pending = true;
        row.command = "claude --old".into();
        row.extra_args = "--old-arg".into();
        crate::session::config::update_config(|config| {
            config
                .session
                .agent_command_override
                .insert("codex".into(), "sleep".into());
            config
                .session
                .agent_extra_args
                .insert("codex".into(), "60".into());
        })?;
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;

        let response = acp_disable(State(state.clone()), Path(row.id.clone()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let stored = crate::session::Storage::open_unwatched("default")?.load()?;
        let stored = stored.iter().find(|r| r.id == row.id).unwrap();
        assert_eq!(stored.view, View::Terminal);
        assert_eq!(stored.tool, "codex");
        assert_eq!(stored.command, "sleep");
        assert_eq!(stored.extra_args, "60");
        assert!(!stored.terminal_rebind_pending);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disable_without_context_retires_an_unborn_producer_without_journal_debt(
    ) -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true
        })?;
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("unborn terminal handoff", project.path().to_str().unwrap());
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.command = "sleep 60".into();
        assert!(
            row.acp_session_id.is_none(),
            "this transition has no context to keep"
        );
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let producer = state
            .acp_supervisor
            .reserve_spawn(
                &row.id,
                crate::acp::supervisor::ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await?;
        let lease = producer.lease().clone();
        lease.set_admission(Some(crate::acp::supervisor::LaunchAdmission {
            store: Arc::new(crate::session::Storage::open_unwatched(
                &row.source_profile,
            )?),
            generation: row.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
        }));
        let cancellation = lease.cancellation().clone();
        let disabling = acp_disable(State(state.clone()), Path(row.id.clone()));
        tokio::pin!(disabling);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                response = &mut disabling => anyhow::bail!("terminal handoff answered before producer settlement: {}", response.into_response().status()),
                _ = cancellation.cancelled() => Ok::<_, anyhow::Error>(()),
            }
        }).await??;
        assert!(futures_util::poll!(&mut disabling).is_pending());
        drop(producer);
        assert!(lease.was_cancelled_before_birth());
        assert!(lease.spawned_runner().is_none());
        assert_eq!(
            lease.prime(),
            crate::acp::runner_lifecycle::PrimeOwnership::NotPrime
        );
        state.acp_supervisor.retry_pending_teardowns().await;
        let response = tokio::time::timeout(std::time::Duration::from_secs(10), disabling)
            .await?
            .into_response();
        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
            anyhow::bail!(
                "no-context terminal handoff failed with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        assert!(response
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        let storage = crate::session::Storage::open_unwatched("default")?;
        let stored = storage.load()?;
        let terminal = stored
            .iter()
            .find(|candidate| candidate.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert!(terminal.lifecycle_generation > row.lifecycle_generation);
        assert!(terminal.lifecycle_reservation.is_none());
        let snapshot = state.runtime.snapshot(&state).await?;
        let terminal = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|candidate| candidate.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert_eq!(
            terminal.agent_pane.state,
            crate::session::PanePresence::Alive
        );
        state.acp_supervisor.forget_session(&row.id)?;
        assert!(state
            .acp_supervisor
            .capture_retirement(&row.id)?
            .lease
            .is_none());
        let next = state
            .acp_supervisor
            .reserve_spawn(
                &row.id,
                crate::acp::supervisor::ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await?;
        drop(next);
        assert!(state
            .acp_supervisor
            .capture_retirement(&row.id)?
            .lease
            .is_none());
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
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
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().unwrap();
        let original_birth = crate::process::process_birth_identity(pid)?
            .expect("owned child has a live process birth identity");
        let fingerprint = original_birth.fingerprint();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(pid);
        let project = tempfile::tempdir()?;
        let mut row = Instance::new("pending terminal handoff", project.path().to_str().unwrap());
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.command = "sleep 60".into();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        let (state, gate, spawner, lease, deleted) =
            pending_handoff(&row, pid, control.clone(), Some(original_birth)).await;
        let original = lease
            .spawned_runner()
            .expect("the original producer recorded its proof");
        assert_eq!(original.pid, pid);
        assert_eq!(original.generation, lease.epoch());
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        assert_eq!(
            crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &worker_registry::load_strict(&row.id)?.unwrap(),
            ),
            original,
        );
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
            (storage.load()?).into(),
            vec![(sampled.clone(), "gated-native-session".into())],
            crate::server::state::StatusSource::DiskOnly,
            during_epoch,
            Ok(during_metadata),
            Default::default(),
        )
        .await;
        assert!(
            state
                .instances
                .read()
                .await
                .iter()
                .find(|session| session.id == row.id)
                .unwrap()
                .is_structured(),
            "reload respects the actual pending handoff exclusion"
        );
        assert!(storage
            .load()?
            .iter()
            .find(|session| session.id == row.id)
            .unwrap()
            .is_structured());
        assert!(storage
            .load()?
            .iter()
            .find(|session| session.id == row.id)
            .unwrap()
            .lifecycle_reservation
            .is_some());
        gate.open.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                response = &mut disabling => panic!(
                    "disable answered before physical retirement: {}",
                    response.into_response().status(),
                ),
                _ = control.terminated() => {}
            }
        })
        .await?;
        assert!(control.signals().contains(&(pid, "TERM")));
        assert!(
            child.try_wait()?.is_none(),
            "the original child still executes before physical retirement"
        );
        assert!(futures_util::poll!(&mut disabling).is_pending());
        child.start_kill()?;
        child.wait().await?;
        control.exit(pid);
        let response = tokio::time::timeout(Duration::from_secs(10), disabling)
            .await?
            .into_response();
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
        assert!(
            !control.is_alive(crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &sampled,
            ))
        );
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
            (persisted).into(),
            vec![(sampled, "gated-native-session".into())],
            crate::server::state::StatusSource::DiskOnly,
            epoch_before_reload,
            Ok(metadata),
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
    #[cfg(unix)]
    async fn disable_request_cancellation_preserves_runtime_owned_handoff() -> anyhow::Result<()> {
        use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
        use crate::process::worker_registry;
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })?;
        let project = tempfile::tempdir()?;
        let mut row = Instance::new(
            "cancelled terminal handoff",
            project.path().to_str().unwrap(),
        );
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.command = "sleep 60".into();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().expect("owned original runner");
        let birth =
            crate::process::process_birth_identity(pid)?.expect("owned original runner birth");
        let fingerprint = birth.fingerprint();
        let mut control_child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let control_pid = control_child.id().expect("independent control process");
        let control_birth = crate::process::process_birth_identity(control_pid)?
            .expect("independent control process birth");
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(pid);
        let (state, gate, spawner, lease, deleted) =
            pending_handoff(&row, pid, control.clone(), Some(birth)).await;
        let original = lease
            .spawned_runner()
            .expect("the original producer recorded its proof");
        assert_eq!(original.pid, pid);
        assert_eq!(original.generation, lease.epoch());
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        assert_eq!(
            crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &worker_registry::load_strict(&row.id)?.unwrap(),
            ),
            original,
        );
        let storage = crate::session::Storage::open_unwatched("default")?;
        let mut peer = Instance::new("independent handoff row", project.path().to_str().unwrap());
        peer.source_profile = "default".into();
        storage.update(|rows, _| {
            rows.push(peer.clone());
            Ok(())
        })?;

        let mut disabling = tokio::spawn({
            let state = state.clone();
            let id = row.id.clone();
            async move { acp_disable(State(state), Path(id)).await.into_response() }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                response = &mut disabling => panic!(
                    "disable answered before original-producer cancellation: {}",
                    response.expect("disable task must not panic").status(),
                ),
                _ = lease.cancellation().cancelled() => {}
            }
        })
        .await?;
        assert!(state.acp_supervisor.test_cancel_requested(&lease));
        let reserved = storage
            .load()?
            .into_iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert!(reserved.is_structured());
        let key = handoff_key(&reserved)?;
        assert!(reserved.lifecycle_reservation_is_owned(
            crate::session::LifecycleOperation::Launch,
            key.generation
        ));
        assert_eq!(
            reserved.terminal_handoff.as_ref().unwrap().stage,
            HandoffStage::RetirementPending
        );
        assert!(child.try_wait()?.is_none());
        assert!(!spawner.is_finished());
        disabling.abort();
        assert!(disabling.await.unwrap_err().is_cancelled());

        // Cancelling the HTTP waiter does not cancel admitted runtime work.
        let durable = storage
            .load()?
            .into_iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert!(durable.is_structured());
        assert_eq!(handoff_key(&durable)?, key);
        assert!(durable.lifecycle_reservation_is_owned(
            crate::session::LifecycleOperation::Launch,
            key.generation
        ));
        assert!(crate::session::try_acquire_terminal_handoff_driver(&row.id)?.is_none());
        let pending = tokio::time::timeout(
            Duration::from_secs(5),
            acp_disable(State(state.clone()), Path(row.id.clone())),
        )
        .await?
        .into_response();
        assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!pending
            .headers()
            .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(
            snapshot
                .value
                .contents
                .sessions
                .iter()
                .find(|session| session.id == row.id)
                .unwrap()
                .view,
            View::Structured
        );

        gate.open.notify_one();
        tokio::time::timeout(Duration::from_secs(5), control.terminated()).await?;
        assert!(
            control.signals().contains(&(pid, "TERM")),
            "runtime-owned work continues after HTTP cancellation"
        );
        assert!(
            child.try_wait()?.is_none(),
            "no physical completion has happened yet"
        );
        let durable = storage
            .load()?
            .into_iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert!(durable.is_structured());
        assert_eq!(handoff_key(&durable)?, key);
        assert!(durable.lifecycle_reservation_is_owned(
            crate::session::LifecycleOperation::Launch,
            key.generation
        ));
        child.start_kill()?;
        child.wait().await?;
        control.exit(pid);
        assert!(tokio::time::timeout(Duration::from_secs(10), spawner)
            .await??
            .is_err());
        tokio::time::timeout(Duration::from_secs(10), state.runtime.work.drain()).await?;
        assert!(deleted.load(Ordering::SeqCst));
        assert!(worker_registry::load_strict(&row.id)?.is_none());
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
        assert!(state
            .acp_supervisor
            .capture_retirement(&row.id)?
            .lease
            .is_none());
        let stored = storage.load()?;
        let terminal = stored.iter().find(|session| session.id == row.id).unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert!(terminal.lifecycle_generation > key.generation);
        assert!(terminal.terminal_handoff.is_none());
        assert!(terminal.lifecycle_reservation.is_none());
        let persisted_peer = stored.iter().find(|session| session.id == peer.id).unwrap();
        assert_eq!(persisted_peer.title, peer.title);
        assert_eq!(persisted_peer.view, peer.view);
        assert_eq!(
            persisted_peer.lifecycle_generation,
            peer.lifecycle_generation
        );
        assert!(persisted_peer.terminal_handoff.is_none());
        assert!(persisted_peer.lifecycle_reservation.is_none());
        assert!(control
            .signals()
            .iter()
            .all(|(signalled, _)| *signalled == pid));
        assert!(
            control_child.try_wait()?.is_none(),
            "the independent process must remain alive"
        );
        assert_eq!(
            crate::process::process_birth_identity(control_pid)?,
            Some(control_birth)
        );

        let response = tokio::time::timeout(
            Duration::from_secs(10),
            acp_disable(State(state.clone()), Path(row.id.clone())),
        )
        .await?
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let epoch = response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER]
            .to_str()?
            .to_owned();
        let revision = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
            .to_str()?
            .parse::<u64>()?;
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(body["view"], "terminal");
        let snapshot = state.runtime.snapshot(&state).await?;
        assert_eq!(snapshot.value.cursor.epoch, epoch);
        assert!(snapshot.value.cursor.revision >= revision);
        let terminal = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|session| session.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert_eq!(
            terminal.agent_pane.state,
            crate::session::PanePresence::Alive
        );
        assert!(snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|session| session.id == peer.id
                && session.view == peer.view
                && session.lifecycle_generation == peer.lifecycle_generation));
        control_child.start_kill()?;
        control_child.wait().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn keep_handoff_resumes_late_original_native_sid_from_its_verified_store(
    ) -> anyhow::Result<()> {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::runner_lifecycle::{test_support::FakeProcessControl, ProcessControl};
        use crate::acp::state::{AcpSessionId, Event};
        use crate::process::worker_registry::{self, ScopeOwnership, WorkerRecord};
        use crate::session::{ConversationBinding, ConversationProvenance, ResumeIntent};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, SystemTime};

        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let _ambient = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        crate::session::config::update_app_state(|s| s.has_acknowledged_agent_hooks = true)?;
        let project = home.path().join("project");
        std::fs::create_dir_all(&project)?;
        let project = project.canonicalize()?;
        let selected = home.path().join("selected-claude");
        let encoded =
            crate::session::capture::encode_claude_project_path(project.to_str().unwrap());
        let transcripts = selected.join("projects").join(&encoded);
        std::fs::create_dir_all(&transcripts)?;
        let selected = selected.canonicalize()?;
        let initial_sid = "11111111-1111-4111-8111-111111111111";
        let final_sid = "22222222-2222-4222-8222-222222222222";
        let initial_transcript = transcripts.join(format!("{initial_sid}.jsonl"));
        let final_transcript = transcripts.join(format!("{final_sid}.jsonl"));
        let initial_bytes = format!(
            "{{\"sessionId\":\"{initial_sid}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"original retained context\"}}}}\n"
        );
        let final_bytes = format!(
            "{{\"sessionId\":\"{final_sid}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"late original producer context\"}}}}\n"
        );
        std::fs::write(&initial_transcript, &initial_bytes)?;
        // A same-SID decoy makes a wrong/default-store resume observably different.
        let default_transcripts = home.path().join(".claude/projects").join(&encoded);
        std::fs::create_dir_all(&default_transcripts)?;
        std::fs::write(
            default_transcripts.join(format!("{final_sid}.jsonl")),
            b"wrong store\n",
        )?;
        let births = home.path().join("terminal-births");
        let resumed = home.path().join("resumed-native-transcript");
        let resumed_sid = home.path().join("resumed-native-sid");
        let quote = |path: &std::path::Path| {
            crate::session::environment::shell_escape(path.to_str().unwrap())
        };
        // This native fixture consumes the launch's actual selector and store.
        // It cannot succeed on a fresh launch or by echoing the expected test ID.
        let script = format!(
            "#!/bin/sh\nset -eu\ncase \"${{1-}}\" in\n  --help) printf '  -r, --resume <id>\\n'; exit 0 ;;\n  --version) printf '2.1.0 (Claude Code)\\n'; exit 0 ;;\nesac\nprintf x >> {}\nsid=\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --resume) [ -z \"$sid\" ] || exit 90; shift; sid=$1 ;;\n  esac\n  shift\ndone\n[ -n \"$sid\" ] || exit 91\ntranscript=\"$CLAUDE_CONFIG_DIR/projects/{encoded}/$sid.jsonl\"\n[ -f \"$transcript\" ] || exit 92\ncp \"$transcript\" {}\nprintf '%s\\n' \"$sid\" > {}\nexec sleep 60\n",
            quote(&births),
            quote(&resumed),
            quote(&resumed_sid),
        );
        let _claude = crate::session::test_support::install_login_shell_path_command(
            home.path(),
            "claude",
            &script,
        );
        let mut row = Instance::new("late native keep handoff", project.to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.command = "claude".into();
        row.acp_session_id = Some(initial_sid.into());
        let capture_floor = SystemTime::UNIX_EPOCH + Duration::from_secs(42);
        row.capture_started_at = Some(capture_floor);
        let native_store = {
            let _selected_environment = crate::session::test_support::EnvGuard::set(&[(
                "CLAUDE_CONFIG_DIR",
                selected.as_os_str(),
            )]);
            row.attested_carry_destination()?
        };
        assert_eq!(native_store.stores, vec![selected.clone()]);
        // The fixture starts with an observed conversation in the very store
        // its live client owns; no asserted or unknown binding grants authority.
        let observed = ConversationBinding {
            session_id: initial_sid.into(),
            execution: Some(native_store.clone()),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        };
        row.agent_session_id = Some(initial_sid.into());
        row.agent_session_binding = Some(observed.clone());
        row.resume_intent = ResumeIntent::Use(initial_sid.into());
        row.resume_binding = Some(observed.clone());
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().expect("owned original producer");
        let birth = crate::process::process_birth_identity(pid)?
            .expect("the original producer is physically alive");
        let fingerprint = birth.fingerprint();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(pid);
        let (mut client, producer_events, commands) =
            AcpClient::fake_for_test_cmd_recording(AcpSessionId(row.id.clone()));
        client.native_store = Some(native_store.clone());
        let client = Arc::new(std::sync::Mutex::new(Some(client)));
        let launches = Arc::new(AtomicUsize::new(0));
        let launched = launches.clone();
        let launcher: crate::acp::supervisor::Launcher = Arc::new(move |config, session_id| {
            launched.fetch_add(1, Ordering::SeqCst);
            let client = client
                .lock()
                .unwrap()
                .take()
                .expect("only the original ACP producer");
            let birth = birth.clone();
            Box::pin(async move {
                let socket = worker_registry::socket_path_for(&session_id.0).unwrap();
                let mut record = WorkerRecord::new(
                    session_id.0.clone(),
                    pid,
                    socket.clone(),
                    "claude-agent-acp".into(),
                    "claude-code".into(),
                    config.cwd,
                    None,
                    vec![],
                    vec![],
                    Some(initial_sid.into()),
                    config.source_profile,
                )
                .with_generation(config.generation);
                record.runner_identity = Some(birth);
                record.scope = ScopeOwnership::Unscoped;
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("original producer lease");
                lease.record_scope(record.scope.clone());
                assert!(lease.record_runner(
                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                ));
                worker_registry::save(&record).unwrap();
                worker_registry::touch_live_socket(&socket);
                Ok(client.with_runner_pid(pid))
            })
        });
        let (state, spawner, lease) =
            handoff_producer_with_launcher(&row, control.clone(), None, launcher).await;
        tokio::time::timeout(Duration::from_secs(5), spawner).await???;
        let original = lease.spawned_runner().expect("captured original producer");
        assert_eq!(original.pid, pid);
        assert_eq!(original.generation, lease.epoch());
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        let mut frames = state.acp_events_tx.subscribe();
        producer_events
            .send(Event::AcpSessionAssigned {
                acp_session_id: initial_sid.into(),
            })
            .await?;
        let initial_frame = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await??;
        assert_eq!(initial_frame.worker_generation, Some(lease.epoch()));
        assert!(
            matches!(initial_frame.event.as_ref(), Event::AcpSessionAssigned { acp_session_id } if acp_session_id == initial_sid)
        );
        assert_eq!(
            state
                .acp_supervisor
                .native_handoff_store(&row.id, initial_sid)
                .await,
            Some(native_store.clone()),
        );
        let storage = crate::session::Storage::open_unwatched("default")?;
        let disabling = acp_disable(State(state.clone()), Path(row.id.clone()));
        tokio::pin!(disabling);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                response = &mut disabling => anyhow::bail!(
                    "Keep handoff answered before physical retirement: {}",
                    response.into_response().status(),
                ),
                _ = control.terminated() => Ok::<_, anyhow::Error>(()),
            }
        })
        .await??;
        let pending = storage
            .load()?
            .into_iter()
            .find(|r| r.id == row.id)
            .unwrap();
        let intent = pending
            .terminal_handoff
            .as_ref()
            .expect("durable Keep handoff");
        assert_eq!(intent.stage, HandoffStage::RetirementPending);
        assert!(
            matches!(&intent.context, HandoffContext::Keep { native_store: selected } if selected == &native_store)
        );
        assert_eq!(intent.source_event_seq, initial_frame.seq);
        assert_eq!(pending.capture_started_at, Some(capture_floor));
        assert_eq!(pending.agent_session_binding, Some(observed.clone()));
        assert_eq!(pending.resume_binding, Some(observed));
        let custody = intent.runner.clone();
        assert_eq!(
            custody,
            crate::session::purge_owners::RunnerCapture::Captured {
                pid,
                generation: original.generation,
                birth_fingerprint: original.birth_fingerprint,
                prime: lease.prime(),
                scopes: lease.scopes(),
            }
        );
        assert!(child.try_wait()?.is_none());
        assert!(control.is_alive(original));
        assert!(
            !births.exists(),
            "no terminal may start before the original dies"
        );

        // The original client, not a session operator, assigns the late ID.
        // The registry owner update is the path RunnerShared uses, and the
        // original drain tags and persists the event while retirement is pending.
        std::fs::write(&final_transcript, &final_bytes)?;
        worker_registry::update_stored_acp_session_id(&row.id, pid, final_sid)?;
        producer_events
            .send(Event::AcpSessionAssigned {
                acp_session_id: final_sid.into(),
            })
            .await?;
        let final_frame = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await??;
        assert_eq!(final_frame.worker_generation, Some(lease.epoch()));
        assert!(
            matches!(final_frame.event.as_ref(), Event::AcpSessionAssigned { acp_session_id } if acp_session_id == final_sid)
        );
        assert!(final_frame.seq > intent.source_event_seq);
        producer_events
            .send(Event::AgentMessageChunk {
                text: "original producer finished materializing its native context".into(),
                message_id: None,
            })
            .await?;
        let tail_frame = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await??;
        assert_eq!(tail_frame.worker_generation, Some(lease.epoch()));
        assert!(tail_frame.seq > final_frame.seq);
        assert!(matches!(
            tail_frame.event.as_ref(),
            Event::AgentMessageChunk { .. }
        ));
        assert!(matches!(
            state.acp_event_store.handoff_context_event(
                &row.id, intent.source_event_seq, tail_frame.seq,
            )?,
            Some((seq, Event::AcpSessionAssigned { acp_session_id }))
                if seq == final_frame.seq && acp_session_id == final_sid
        ));
        let record =
            worker_registry::load_strict(&row.id)?.expect("original still owns its record");
        assert!(original.owns_record(&record));
        assert_eq!(record.stored_acp_session_id.as_deref(), Some(final_sid));
        let still_pending = storage
            .load()?
            .into_iter()
            .find(|r| r.id == row.id)
            .unwrap();
        let held = still_pending.terminal_handoff.as_ref().unwrap();
        assert_eq!(held.stage, HandoffStage::RetirementPending);
        assert_eq!(held.runner, custody);
        assert_eq!(held.source_event_seq, intent.source_event_seq);
        assert!(
            matches!(&held.context, HandoffContext::Keep { native_store: selected } if selected == &native_store)
        );
        assert_eq!(still_pending.capture_started_at, Some(capture_floor));
        assert_eq!(
            still_pending.agent_session_binding,
            pending.agent_session_binding
        );
        assert_eq!(still_pending.resume_binding, pending.resume_binding);
        assert!(child.try_wait()?.is_none());
        assert!(futures_util::poll!(&mut disabling).is_pending());
        assert!(!births.exists());

        child.start_kill()?;
        child.wait().await?;
        control.exit(pid);
        let response = tokio::time::timeout(Duration::from_secs(10), disabling)
            .await?
            .into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        tokio::time::timeout(Duration::from_secs(5), state.runtime.work.drain()).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !std::fs::read_to_string(&resumed_sid).is_ok_and(|sid| sid.trim() == final_sid) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(std::fs::read_to_string(&resumed_sid)?.trim(), final_sid);
        assert_eq!(std::fs::read_to_string(&resumed)?, final_bytes);
        assert_eq!(std::fs::read_to_string(&initial_transcript)?, initial_bytes);
        assert_eq!(std::fs::read_to_string(&final_transcript)?, final_bytes);
        assert_eq!(
            std::fs::read(&births)?,
            b"x",
            "exactly one native terminal birth"
        );
        let terminal = storage
            .load()?
            .into_iter()
            .find(|r| r.id == row.id)
            .unwrap();
        assert_eq!(terminal.view, View::Terminal);
        assert_eq!(terminal.agent_session_id.as_deref(), Some(final_sid));
        assert_eq!(
            terminal.agent_session_binding.as_ref().unwrap().provenance,
            ConversationProvenance::Observed
        );
        let (selected_sid, selected_execution) = terminal
            .selected_claude_conversation()
            .expect("a verified native resume target, not null DTO fields");
        assert_eq!(selected_sid, final_sid);
        assert_eq!(selected_execution, &native_store);
        assert!(terminal
            .capture_started_at
            .is_some_and(|floor| floor > capture_floor));
        let pane = crate::tmux::test_helpers::only_pane_id(&crate::tmux::Session::generate_name(
            &row.id, &row.title,
        ));
        let terminal_pid =
            crate::tmux::test_helpers::pane_field(&pane, "#{pane_pid}").parse::<u32>()?;
        let terminal_birth = crate::process::process_birth_identity(terminal_pid)?
            .expect("the resumed native terminal has a real live process birth");
        assert_ne!(terminal_birth.fingerprint(), fingerprint);
        assert!(
            !crate::session::capture::claude_host_transcript_confirmed_absent(
                &selected_execution.cwd.to_string_lossy(),
                selected_sid,
                &[],
                Some(&selected_execution.stores[0]),
            )
        );
        assert!(terminal.terminal_handoff.is_none());
        assert!(terminal.lifecycle_reservation.is_none());
        assert!(worker_registry::load_strict(&row.id)?.is_none());
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
        assert!(!control.is_alive(original));
        assert_eq!(
            launches.load(Ordering::SeqCst),
            1,
            "no replacement Structured actor"
        );
        let commands = commands.lock().unwrap();
        assert!(commands.contains(&"close_session"));
        assert!(commands.contains(&"shutdown"));
        assert!(
            !commands.contains(&"delete_session"),
            "Keep retains native history"
        );
        assert!(
            !commands.contains(&"reset_session"),
            "no fresh ACP fallback"
        );
        Ok(())
    }

    #[cfg(unix)]
    async fn retired_native_handoff_with_event_store(
        row: &Instance,
        event_store: Arc<crate::acp::event_store::EventStore>,
    ) -> anyhow::Result<Arc<AppState>> {
        use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().expect("owned original producer");
        let birth = crate::process::process_birth_identity(pid)?.expect("original producer birth");
        let fingerprint = birth.fingerprint();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(pid);
        let (state, gate, spawner, lease, deleted) = pending_handoff_with_event_store(
            row,
            pid,
            control.clone(),
            Some(birth),
            Some(event_store),
        )
        .await;
        let original = lease
            .spawned_runner()
            .expect("captured original physical producer");
        assert_eq!(original.pid, pid);
        assert_eq!(original.generation, lease.epoch());
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        assert_eq!(
            crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &crate::process::worker_registry::load_strict(&row.id)?.unwrap(),
            ),
            original
        );
        let (native, expected) = load_handoff_row(&state, &row.source_profile, &row.id).await?;
        let driver =
            Arc::new(crate::session::try_acquire_terminal_handoff_driver(&row.id)?.unwrap());
        let reserve_state = state.clone();
        let floor = state.acp_event_store.highest_seq_checked(&row.id)?;
        let owner = tokio::task::spawn_blocking(move || {
            reserve_handoff_in(
                native,
                &expected,
                HandoffContext::Discard,
                floor,
                reserve_state,
                driver,
            )
        })
        .await??;
        let key = owner.key.unwrap();
        let intent = crate::session::Storage::open_unwatched(&row.source_profile)?
            .load()?
            .into_iter()
            .find(|r| r.id == row.id)
            .unwrap()
            .terminal_handoff
            .unwrap();
        let mut target = state.acp_supervisor.capture_retirement(&row.id)?;
        intent.ensure_retirement_target(&row.id, &mut target)?;
        let stop_state = state.clone();
        let stop_id = row.id.clone();
        let stop_target = target.clone();
        let stopping = tokio::spawn(async move {
            stop_state
                .acp_supervisor
                .shutdown_captured_and_delete_and_wait(
                    &stop_id,
                    &stop_target,
                    Duration::from_secs(5),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), lease.cancellation().cancelled()).await?;
        gate.open.notify_one();
        tokio::time::timeout(Duration::from_secs(5), control.terminated()).await?;
        assert!(
            child.try_wait()?.is_none(),
            "signals alone cannot prove physical retirement"
        );
        child.start_kill()?;
        child.wait().await?;
        control.exit(pid);
        stopping.await??;
        assert!(spawner.await?.is_err());
        assert!(deleted.load(Ordering::SeqCst));
        assert!(control
            .signals()
            .iter()
            .all(|(signalled, _)| *signalled == pid));
        let completed = intent.completed_runner(&row.id, &target)?;
        let final_seq = state.acp_event_store.highest_seq_checked(&row.id)?;
        let final_event = state
            .acp_event_store
            .handoff_context_event(&row.id, floor, final_seq)?
            .map(|(_, event)| event);
        let save_id = row.id.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _title = crate::session::acquire_session_title_lock(&save_id)?;
            let _lifecycle = owner
                .store()
                .storage()
                .acquire_instance_lifecycle_lock(&save_id)?;
            record_retired_handoff_in(
                owner.store(),
                &save_id,
                key,
                completed,
                final_seq,
                final_event,
            )?;
            commit_terminal_in(owner.store(), &save_id, key)?;
            // Release exclusion before the owner's tracked exact-generation cleanup.
            drop(_lifecycle);
            drop(_title);
            drop(owner);
            Ok(())
        })
        .await??;
        state.acp_supervisor.acknowledge_retirement(&target)?;
        tokio::time::timeout(Duration::from_secs(5), state.runtime.work.drain()).await?;
        assert!(crate::process::worker_registry::load_strict(&row.id)?.is_none());
        assert!(state
            .acp_supervisor
            .capture_retirement(&row.id)?
            .lease
            .is_none());
        Ok(state)
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn durable_handoff_reconstruction_preserves_launch_and_checked_cleanup(
    ) -> anyhow::Result<()> {
        use clap::Parser;
        use futures_util::FutureExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        #[derive(Parser)]
        struct SessionCli {
            #[command(subcommand)]
            command: crate::cli::session::SessionCommands,
        }
        fn projection_counts(db: &rusqlite::Connection, id: &str) -> anyhow::Result<[i64; 4]> {
            let mut result = [0; 4];
            for (index, table) in [
                "acp_events",
                "acp_attachments",
                "acp_pending_attachments",
                "acp_rate_limit_budgets",
            ]
            .iter()
            .enumerate()
            {
                result[index] = db.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                    [id],
                    |r| r.get(0),
                )?;
            }
            Ok(result)
        }
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|s| s.has_acknowledged_agent_hooks = true)?;
        for owed in [
            HandoffStage::TerminalLaunchOwed,
            HandoffStage::ProjectionCleanupOwed,
        ] {
            let project = tempfile::tempdir()?;
            let db_path = project.path().join("original-events.db");
            let events = Arc::new(crate::acp::event_store::EventStore::open(&db_path, 100)?);
            let entered = project.path().join("hook-entered");
            let release = project.path().join("hook-release");
            let hooks = project.path().join("hook-count");
            let births = project.path().join("terminal-births");
            let quote = |p: &std::path::Path| {
                format!("'{}'", p.display().to_string().replace('\'', "'\"'\"'"))
            };
            let hook = format!(
                "printf x >> {}; : > {}; while [ ! -e {} ]; do sleep 0.01; done",
                quote(&hooks),
                quote(&entered),
                quote(&release)
            );
            crate::session::config::update_config(|config| config.hooks.on_launch = vec![hook])?;
            let mut source =
                Instance::new("durable native handoff", project.path().to_str().unwrap());
            source.source_profile = "default".into();
            source.view = View::Structured;
            source.command = format!(
                "sh -c {}",
                quote(std::path::Path::new(&format!(
                    "printf x >> {}; exec sleep 60",
                    quote(&births),
                )))
            );
            let pane_name = crate::tmux::Session::generate_name(&source.id, &source.title);
            let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(&pane_name);
            let state = retired_native_handoff_with_event_store(&source, events.clone()).await?;
            let storage = crate::session::Storage::open_unwatched("default")?;
            let mut stopped = Instance::new(
                "deliberately stopped control",
                project.path().to_str().unwrap(),
            );
            stopped.source_profile = "default".into();
            stopped.view = View::Terminal;
            stopped.status = Status::Stopped;
            storage.update(|rows, _| {
                rows.push(stopped.clone());
                Ok(())
            })?;
            state.instances.write().await.push(stopped.clone());
            let db = rusqlite::Connection::open(&db_path)?;
            for id in [&source.id, &stopped.id] {
                events.record(
                    id,
                    events.highest_seq_checked(id)? + 1,
                    &crate::acp::Event::Stopped {
                        reason: "projection sentinel".into(),
                    },
                )?;
                db.execute("INSERT INTO acp_attachments (session_id, seq, attachment_id, kind, mime_type, name, data, created_at) VALUES (?1,1,'kept','image','image/png',NULL,X'01',0)", [id])?;
                db.execute("INSERT INTO acp_pending_attachments (session_id, ref_id, attachment_id, kind, mime_type, name, data, created_at) VALUES (?1,'ref','pending','image','image/png',NULL,X'02',0)", [id])?;
                db.execute("INSERT OR REPLACE INTO acp_rate_limit_budgets (session_id, spent, armed) VALUES (?1,2,1)", [id])?;
            }
            let counts = projection_counts(&db, &source.id)?;
            let control_counts = projection_counts(&db, &stopped.id)?;
            let (native, mut launch) = load_handoff_row(&state, "default", &source.id).await?;
            let key = handoff_key(&launch)?;
            assert_eq!(
                launch.terminal_handoff.as_ref().unwrap().stage,
                HandoffStage::TerminalLaunchOwed
            );
            let driver =
                Arc::new(crate::session::try_acquire_terminal_handoff_driver(&source.id)?.unwrap());
            let adoption_state = state.clone();
            let launching = tokio::task::spawn_blocking(move || -> anyhow::Result<Instance> {
                launch.start_terminal_handoff_in(&native, key, &driver, true)?;
                assert!(launch.tmux_session()?.exists_live());
                // Reject canonical adoption after the physical terminal starts.
                crate::server::session_store::before_next_adoption_for_test(move || {
                    adoption_state
                        .canonical_metadata
                        .blocking_write()
                        .profiles
                        .retain(|profile| profile.name != "default");
                });
                let current_key = handoff_key(&launch)?;
                let identity = native.storage().bound_profile_identity()?;
                let error = (&native as &dyn SessionStore)
                    .update(|rows, _| {
                        let row = rows.iter_mut().find(|row| row.id == launch.id).unwrap();
                        matching_handoff(row, "default", identity, current_key)?.stage = owed;
                        Ok(())
                    })
                    .expect_err("missing committed profile must reject canonical adoption");
                assert!(error.is::<crate::session::SessionCommitApplied>());
                Ok(launch)
            });
            let held_checks = std::panic::AssertUnwindSafe(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !entered.exists() {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
                assert!(!launching.is_finished());
                assert!(!source.tmux_session()?.exists_live());
                let before = storage
                    .load()?
                    .into_iter()
                    .find(|row| row.id == source.id)
                    .unwrap();
                let live_key = handoff_key(&before)?;
                storage.update(|rows, _| {
                    let current = rows.iter_mut().find(|row| row.id == source.id).unwrap();
                    assert_eq!(handoff_key(current)?, live_key);
                    current.lifecycle_reservation.as_mut().unwrap().at = chrono::Utc::now()
                        - Instance::LIFECYCLE_RESERVATION_TTL
                        - chrono::Duration::seconds(1);
                    Ok(())
                })?;
                // The blocked real host hook proves the live driver, not the marker age.
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(
                        recover_pending_terminal_handoffs(&state, HandoffRecovery::Maintenance),
                        recover_pending_terminal_handoffs(&state, HandoffRecovery::Startup),
                    );
                    let parsed = SessionCli::try_parse_from([
                        "aoe-session",
                        "set-session-id",
                        &source.id,
                        "manual-native-id",
                    ])?;
                    let result = crate::cli::session::run("default", parsed.command).await;
                    anyhow::ensure!(result.is_err(), "CLI mutation bypassed the live handoff");
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
                let held = storage
                    .load()?
                    .into_iter()
                    .find(|row| row.id == source.id)
                    .unwrap();
                assert_eq!(handoff_key(&held)?, live_key);
                assert_eq!(
                    held.terminal_handoff.as_ref().unwrap().target_generation,
                    before.terminal_handoff.as_ref().unwrap().target_generation
                );
                assert_eq!(
                    held.terminal_handoff.as_ref().unwrap().stage,
                    HandoffStage::TerminalLaunchOwed
                );
                assert_eq!(std::fs::read(&hooks)?, b"x");
                assert!(!births.exists());
                assert!(crate::session::try_acquire_terminal_handoff_driver(&source.id)?.is_none());
                Ok::<_, anyhow::Error>(())
            })
            .catch_unwind()
            .await;
            std::fs::write(&release, b"release")?;
            let launched = tokio::time::timeout(Duration::from_secs(10), launching).await???;
            match held_checks {
                Ok(result) => result?,
                Err(panic) => std::panic::resume_unwind(panic),
            }
            state.runtime.work.drain().await;
            let persisted = storage
                .load()?
                .into_iter()
                .find(|row| row.id == source.id)
                .unwrap();
            let intent = persisted.terminal_handoff.as_ref().unwrap();
            assert_eq!(intent.id, key.id);
            assert_eq!(intent.stage, owed);
            intent.ensure_original_stopped(&source.id)?;
            let attempt_generation = intent
                .launch
                .as_ref()
                .expect("real launch checkpoint")
                .generation;
            let target_generation = intent.target_generation;
            let pane_id = crate::tmux::test_helpers::only_pane_id(&pane_name);
            let terminal_pid =
                crate::tmux::test_helpers::pane_field(&pane_id, "#{pane_pid}").parse::<u32>()?;
            let terminal_birth = crate::process::process_birth_identity(terminal_pid)?
                .expect("real terminal child birth");
            tokio::time::timeout(Duration::from_secs(5), async {
                while !std::fs::metadata(&births).is_ok_and(|metadata| metadata.len() == 1) {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(std::fs::read(&births)?, b"x");
            assert_eq!(std::fs::read(&hooks)?, b"x");
            db.execute_batch(&format!("CREATE TRIGGER refuse_handoff_delete BEFORE DELETE ON acp_rate_limit_budgets WHEN OLD.session_id = '{}' BEGIN SELECT RAISE(ABORT, 'durable handoff cleanup blocked'); END;", source.id))?;
            drop(launched);
            drop(events);
            drop(state);
            let mut rows = storage.load()?;
            for row in &mut rows {
                row.source_profile = "default".into();
            }
            let new_events = Arc::new(crate::acp::event_store::EventStore::open(&db_path, 100)?);
            let structured_launches = Arc::new(AtomicUsize::new(0));
            let launches = structured_launches.clone();
            let launcher: crate::acp::supervisor::Launcher = Arc::new(move |_, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "unexpected structured launch during terminal recovery".into(),
                    ))
                })
            });
            let recovered = crate::server::test_support::build_test_app_state_impl_with_event_store(
                rows,
                Vec::new(),
                Vec::new(),
                None,
                Some(new_events.clone()),
                |_| {},
                |supervisor| supervisor.with_launcher(launcher),
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&recovered).await;
            if owed == HandoffStage::TerminalLaunchOwed {
                let marker = crate::tmux::env::AOE_TERMINAL_HANDOFF_ID_KEY;
                let wrong_id = uuid::Uuid::new_v4().to_string();
                assert!(crate::tmux::tmux_command()
                    .args(["set-environment", "-t", &pane_name, marker, &wrong_id,])
                    .output()?
                    .status
                    .success());
                tokio::time::timeout(
                    Duration::from_secs(10),
                    recover_pending_terminal_handoffs(&recovered, HandoffRecovery::Startup),
                )
                .await?;
                let refused = storage
                    .load()?
                    .into_iter()
                    .find(|row| row.id == source.id)
                    .unwrap();
                assert_eq!(refused.terminal_handoff.as_ref().unwrap().id, key.id);
                assert_eq!(refused.terminal_handoff.as_ref().unwrap().stage, owed);
                assert_eq!(projection_counts(&db, &source.id)?, counts);
                assert_eq!(crate::tmux::test_helpers::only_pane_id(&pane_name), pane_id);
                assert_eq!(
                    crate::process::process_birth_identity(terminal_pid)?,
                    Some(terminal_birth.clone())
                );
                assert_eq!(std::fs::read(&hooks)?, b"x");
                assert_eq!(std::fs::read(&births)?, b"x");
                assert!(crate::tmux::tmux_command()
                    .args([
                        "set-environment",
                        "-t",
                        &pane_name,
                        marker,
                        &key.id.to_string(),
                    ])
                    .output()?
                    .status
                    .success());
            }
            tokio::time::timeout(
                Duration::from_secs(10),
                recover_pending_terminal_handoffs(&recovered, HandoffRecovery::Startup),
            )
            .await?;
            let pending = storage
                .load()?
                .into_iter()
                .find(|row| row.id == source.id)
                .unwrap();
            let pending_intent = pending
                .terminal_handoff
                .as_ref()
                .expect("SQL failure keeps durable debt");
            assert_eq!(pending_intent.id, key.id);
            assert_eq!(pending_intent.stage, HandoffStage::ProjectionCleanupOwed);
            assert_eq!(
                pending_intent.launch.as_ref().unwrap().generation,
                attempt_generation
            );
            assert_eq!(pending_intent.target_generation, target_generation);
            assert_eq!(
                projection_counts(&db, &source.id)?,
                counts,
                "last-table SQL failure rolls back every earlier DELETE"
            );
            assert_eq!(crate::tmux::test_helpers::only_pane_id(&pane_name), pane_id);
            assert_eq!(
                crate::process::process_birth_identity(terminal_pid)?,
                Some(terminal_birth.clone())
            );
            assert_eq!(std::fs::read(&hooks)?, b"x");
            assert_eq!(std::fs::read(&births)?, b"x");
            let unavailable = acp_disable(State(recovered.clone()), Path(source.id.clone()))
                .await
                .into_response();
            assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(!unavailable
                .headers()
                .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
            assert_eq!(projection_counts(&db, &source.id)?, counts);
            db.execute_batch("DROP TRIGGER refuse_handoff_delete")?;
            tokio::time::timeout(
                Duration::from_secs(10),
                recover_pending_terminal_handoffs(&recovered, HandoffRecovery::Maintenance),
            )
            .await?;
            let complete = storage
                .load()?
                .into_iter()
                .find(|row| row.id == source.id)
                .unwrap();
            assert!(complete.terminal_handoff.is_none());
            assert!(complete.lifecycle_reservation.is_none());
            assert_eq!(projection_counts(&db, &source.id)?, [0; 4]);
            assert_eq!(projection_counts(&db, &stopped.id)?, control_counts);
            assert_eq!(new_events.highest_seq_checked(&source.id)?, 0);
            assert_eq!(structured_launches.load(Ordering::SeqCst), 0);
            assert_eq!(crate::tmux::test_helpers::only_pane_id(&pane_name), pane_id);
            assert_eq!(
                crate::process::process_birth_identity(terminal_pid)?,
                Some(terminal_birth)
            );
            assert_eq!(std::fs::read(&hooks)?, b"x");
            assert_eq!(std::fs::read(&births)?, b"x");
            let control = storage
                .load()?
                .into_iter()
                .find(|row| row.id == stopped.id)
                .unwrap();
            assert_eq!(control.view, stopped.view);
            assert_eq!(control.status, stopped.status);
            assert_eq!(control.lifecycle_generation, stopped.lifecycle_generation);
            assert!(control.terminal_handoff.is_none());
            assert!(!stopped.tmux_session()?.exists_live());
            let generation = complete.lifecycle_generation;
            let response = acp_disable(State(recovered.clone()), Path(source.id.clone()))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response
                .headers()
                .contains_key(crate::daemon::RUNTIME_REVISION_HEADER));
            assert_eq!(
                storage
                    .load()?
                    .into_iter()
                    .find(|row| row.id == source.id)
                    .unwrap()
                    .lifecycle_generation,
                generation
            );
            assert_eq!(std::fs::read(&hooks)?, b"x");
            assert_eq!(std::fs::read(&births)?, b"x");
            recovered.runtime.work.drain().await;
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn disable_refuses_success_and_retains_unproven_retirement() -> anyhow::Result<()> {
        use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
        use crate::process::worker_registry;
        use std::time::Duration;
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true
        })?;
        let project = tempfile::tempdir()?;
        let mut row = Instance::new(
            "unproven terminal handoff",
            project.path().to_str().unwrap(),
        );
        row.view = View::Structured;
        row.source_profile = "default".into();
        row.command = "sleep 60".into();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
            crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().expect("owned original runner");
        let birth =
            crate::process::process_birth_identity(pid)?.expect("owned original runner birth");
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(pid);
        let fingerprint = birth.fingerprint();
        let (state, gate, spawner, lease, _) =
            pending_handoff(&row, pid, control.clone(), Some(birth)).await;
        let original = lease
            .spawned_runner()
            .expect("the original producer recorded its proof");
        assert_eq!(original.pid, pid);
        assert_eq!(original.generation, lease.epoch());
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        assert_eq!(
            crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &worker_registry::load_strict(&row.id)?.unwrap(),
            ),
            original,
        );
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
        assert!(control.signals().contains(&(pid, "KILL")));
        assert!(
            child.try_wait()?.is_none(),
            "the unretired original remains physically alive"
        );
        assert!(spawner.await?.is_err());
        assert!(state
            .instances
            .read()
            .await
            .iter()
            .find(|session| session.id == row.id)
            .unwrap()
            .is_structured());
        let storage = crate::session::Storage::open_unwatched("default")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let durable = storage
                .load()?
                .into_iter()
                .find(|session| session.id == row.id)
                .unwrap();
            assert!(
                durable.is_structured(),
                "unproven retirement cannot publish Terminal"
            );
            if durable.lifecycle_reservation.is_none() {
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "owned handoff token was not released"
            );
            tokio::task::yield_now().await;
        }
        child.start_kill()?;
        child.wait().await?;
        control.exit(pid);
        state.acp_supervisor.retry_pending_teardowns().await;
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
        assert!(worker_registry::load(&row.id)?.is_none());
        let response = acp_disable(State(state.clone()), Path(row.id.clone()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let rows = state.instances.read().await;
        let terminal = rows.iter().find(|session| session.id == row.id).unwrap();
        assert!(!terminal.is_structured());
        assert!(terminal.lifecycle_reservation.is_none());
        assert_eq!(
            terminal.agent_pane.state,
            crate::session::PanePresence::Alive
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enabling_a_stopped_terminal_clears_only_its_observed_native_park() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for case in ["fresh", "superseded", "overflow"] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new(case, project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Terminal;
            row.status = Status::Stopped;
            row.tool = "claude".into();
            row.agent_name = Some("claude".into());
            row.mark_idle_dormant();
            row.lifecycle_generation = if case == "overflow" { u64::MAX } else { 14 };
            let mut persisted = row.clone();
            if case == "superseded" {
                persisted.lifecycle_generation += 1;
            }
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![persisted.clone()],
            );
            let births = Arc::new(AtomicUsize::new(0));
            let calls = births.clone();
            let state = crate::server::test_support::build_test_app_state_with_launcher(
                vec![row.clone()],
                Arc::new(move |_, _| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        assert!(tokio::process::Command::new("true")
                            .status()
                            .await
                            .unwrap()
                            .success());
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(crate::acp::acp_client::AcpError::Spawn(
                            "controlled post-admission launch failure".into(),
                        ))
                    })
                }),
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let mut working = row.clone();
            let namespace = state.profile_namespace.clone().read_owned().await;
            let result = commit_structured_view(&state, &mut working, None, namespace).await;
            let storage = crate::session::Storage::open_unwatched("default").unwrap();
            if case != "fresh" {
                assert!(result.is_err(), "{case}");
                let stored = storage.load().unwrap().remove(0);
                assert_eq!(stored.view, View::Terminal);
                assert_eq!(stored.lifecycle_generation, persisted.lifecycle_generation);
                assert_eq!(stored.status, Status::Stopped);
                assert!(stored.is_idle_dormant());
                assert_eq!(births.load(Ordering::SeqCst), 0);
                continue;
            }
            assert!(result.is_ok());
            assert_eq!(working.view, View::Structured);
            assert_eq!(working.status, Status::Idle);
            assert!(!working.is_idle_dormant());
            assert_eq!(working.lifecycle_generation, row.lifecycle_generation + 1);
            let exclusion =
                crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &row.id, true)
                    .await
                    .unwrap();
            let (request, reservation) =
                deferred_enable_request(&state, &working, "claude".into(), None, exclusion)
                    .await
                    .expect("fresh enable target")
                    .unwrap();
            assert!(state
                .acp_supervisor
                .spawn_inner(request, reservation)
                .await
                .is_err());
            assert_eq!(births.load(Ordering::SeqCst), 1);

            let response =
                crate::server::api::acp::shutdown_acp(State(state.clone()), Path(row.id.clone()))
                    .await
                    .into_response();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let stopped = storage.load().unwrap().remove(0);
            assert!(stopped.lifecycle_generation > working.lifecycle_generation);
            let exclusion =
                crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &row.id, true)
                    .await
                    .unwrap();
            assert!(
                deferred_enable_request(&state, &working, "claude".into(), None, exclusion)
                    .await
                    .is_none()
            );
            let retained = storage.load().unwrap().remove(0);
            assert_eq!(retained.lifecycle_generation, stopped.lifecycle_generation);
            assert_eq!(retained.status, Status::Stopped);
            assert!(retained.is_idle_dormant());
            assert_eq!(births.load(Ordering::SeqCst), 1);
        }
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

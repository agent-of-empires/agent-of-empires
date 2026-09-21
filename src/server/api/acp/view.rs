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
        };
    }
    if crate::agents::acp_transcript_cli_resumable(tool, acp_agent) && transcript_present {
        if let Some(id) = agent_session_id.filter(|s| !s.trim().is_empty()) {
            return StructuredSeed {
                stored_acp_session_id: Some(id.to_string()),
                seed_history_replay: true,
            };
        }
    }
    StructuredSeed {
        stored_acp_session_id: None,
        seed_history_replay: import_pending,
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
    if instance.is_structured() {
        return view_response(id, View::Structured);
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
    if let Err(resp) = commit_structured_view(&state, &mut instance).await {
        return resp;
    }
    spawn_enabled_worker(state.clone(), inst_lock.clone(), instance, agent_name);
    drop(transition_guard);
    view_response(id, View::Structured)
}

/// Kill the tmux side and persist the structured view, then mirror it into
/// memory unless a newer lifecycle generation already landed.
async fn commit_structured_view(state: &AppState, instance: &mut Instance) -> Result<(), Response> {
    let inst_for_transition = instance.clone();
    let profile = instance.source_profile.clone();
    let file_watch = state.file_watch.clone();
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let storage = crate::session::Storage::new(&profile, file_watch)?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&inst_for_transition.id)
            .map_err(|error| {
                anyhow::anyhow!("failed to acquire terminal-to-ACP lifecycle lock: {error}")
            })?;
        if let Err(e) = inst_for_transition.kill_locked() {
            tracing::warn!(target: "acp.switch", session = %inst_for_transition.id, "kill tmux failed: {e}");
        }
        inst_for_transition.kill_ancillary_tmux_sessions_locked();
        storage.update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == inst_for_transition.id)
            else {
                anyhow::bail!("session disappeared during terminal-to-ACP transition");
            };
            slot.view = View::Structured;
            slot.resume_intent = ResumeIntent::Default;
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
        inst.status = Status::Idle;
        inst.lifecycle_generation = lifecycle_generation;
        inst.acp_load_session_capable = None;
    };
    apply(instance);
    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|candidate| candidate.id == id) {
        if lifecycle_generation >= slot.lifecycle_generation {
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
) {
    // A resumable terminal transcript can only be probed on the host; a
    // sandboxed session attempts the seeded load unconditionally.
    let transcript_present = instance.is_sandboxed()
        || instance
            .agent_session_id
            .as_deref()
            .map(|sid| {
                !crate::session::capture::claude_host_transcript_confirmed_absent(
                    &instance.project_path,
                    sid,
                    &instance.resolved_host_environment(),
                    instance
                        .declared_agent_config_dir_for(&instance.tool)
                        .as_deref(),
                )
            })
            .unwrap_or(false);
    let seed = resolve_structured_seed(
        &instance.tool,
        &agent_name,
        instance.acp_session_id.as_deref(),
        instance.agent_session_id.as_deref(),
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
            .any(|candidate| candidate.id == session_id && candidate.is_structured());
        if !still_structured {
            return;
        }
        let supervisor = &state.acp_supervisor;
        let sandbox_info = match crate::acp::sandbox::ensure_container_for_session_locked(
            &state.instances,
            &session_id,
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
            ..spawn_request_for(&instance, agent_name.clone(), sandbox_info)
        };
        if let Err(e) = supervisor.spawn(request).await {
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

    if !instance.is_structured() {
        // A reload may have cached a pre-enable terminal snapshot; trust the
        // durable row before answering idempotently.
        match load_persisted_instance(&state, &profile, &id).await {
            Ok(Some(durable)) if durable.is_structured() => {
                instance = adopt_persisted_structured_instance(instance, durable, &profile);
            }
            Ok(Some(_)) => return view_response(id, View::Terminal),
            Ok(None) => return session_not_found(),
            Err(resp) => return resp,
        }
    }

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
        instance.switch_to_terminal_keep_context();
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

    persist_terminal_view(&state, &instance, &profile, keep_context).await;

    // Committed before shutdown so the reconciler cannot respawn a worker in
    // the teardown window.
    let shutdown_result = if keep_context {
        state.acp_supervisor.shutdown(&id).await
    } else {
        state.acp_supervisor.shutdown_and_delete(&id).await
    };
    match shutdown_result {
        Ok(()) | Err(SupervisorError::UnknownSession(_)) => {}
        Err(e) => {
            tracing::warn!(target: "acp.switch", session = %id, "shutdown structured view failed: {e}");
        }
    }
    // The tmux pane reprints a kept conversation, so the ACP projection goes.
    state.acp_supervisor.forget_session(&id);
    state.acp_event_store.delete_session(&id);

    match tokio::task::spawn_blocking(move || instance.start()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "acp.switch", session = %id, "tmux start after disable: {e}");
        }
        Err(e) => {
            tracing::error!(target: "acp.switch", session = %id, "spawn_blocking failed: {e}");
        }
    }
    view_response(id, View::Terminal)
}

async fn load_persisted_instance(
    state: &AppState,
    profile: &str,
    id: &str,
) -> Result<Option<Instance>, Response> {
    let id_for_load = id.to_string();
    let profile_for_load = profile.to_string();
    let file_watch = state.file_watch.clone();
    let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Instance>> {
        let storage = crate::session::Storage::new(&profile_for_load, file_watch)?;
        Ok(storage
            .load()?
            .into_iter()
            .find(|candidate| candidate.id == id_for_load))
    })
    .await;
    match persisted {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(error)) => {
            tracing::error!(target: "acp.switch", session = %id, "load before disable: {error:#}");
            Err(internal_error("failed to read session state"))
        }
        Err(join_error) => {
            tracing::error!(target: "acp.switch", session = %id, "load before disable panicked: {join_error}");
            Err(internal_error("failed to read session state"))
        }
    }
}

/// Persist the terminal view, then mirror it into memory with an epoch bump so
/// an older disk snapshot cannot be reloaded over it.
async fn persist_terminal_view(
    state: &AppState,
    instance: &Instance,
    profile: &str,
    keep_context: bool,
) {
    let apply = move |slot: &mut Instance, from: &Instance| {
        slot.view = View::Terminal;
        slot.acp_session_id = from.acp_session_id.clone();
        slot.import_pending = from.import_pending;
        if keep_context {
            slot.agent_session_id = from.agent_session_id.clone();
            slot.resume_intent = from.resume_intent.clone();
        }
    };
    let snapshot = instance.clone();
    let profile_for_save = profile.to_string();
    let file_watch = state.file_watch.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::new(&profile_for_save, file_watch)?;
        storage.update(|all, _groups| {
            if let Some(slot) = all.iter_mut().find(|candidate| candidate.id == snapshot.id) {
                apply(slot, &snapshot);
            }
            Ok(())
        })?;
        Ok(())
    })
    .await;
    match save_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(target: "acp.switch", "save after disable: {e}"),
        Err(join_err) => {
            tracing::error!(target: "acp.switch", "save task panicked after disable: {join_err}")
        }
    }
    let mut instances = state.instances.write().await;
    if let Some(slot) = instances.iter_mut().find(|i| i.id == instance.id) {
        apply(slot, instance);
        slot.acp_load_session_capable = None;
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
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

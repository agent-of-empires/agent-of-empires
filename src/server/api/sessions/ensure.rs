//! The ensure-* lifecycle endpoints and terminal attach/kill.

use super::*;

/// Ensure the main agent tmux session is alive, restarting it if dead.
///
/// Mirrors the TUI's `attach_session` restart logic: checks the actual tmux
/// state (exists / pane dead / running unexpected shell) and restarts the
/// instance when needed. Returns the resulting status so the frontend can
/// decide whether to proceed with the WebSocket attach.
///
/// Concurrency: a per-instance `tokio::sync::Mutex` serializes ensure calls
/// for the same session so two rapid POSTs don't both decide "dead" and race
/// on `tmux new-session`.
///
/// Read-only: in read-only mode, the endpoint may report `alive` but will
/// refuse to kill+restart a session. Returns 403 when a restart is needed.
///
/// Latency: bounded by `RESUME_PROBE_MAX` (~3s) per probe.
///   * No-op (pane alive): inspect-only, ~tmux RTT.
///   * Healthy resume: Tier-1 probe only, returns after the
///     `RESUME_PROBE_POST_SHELL_GRACE` (~2s) shortcut. Shell-wrapper
///     overrides charitably burn the full ~3s instead (see
///     `Instance::probe_settle`).
///   * Probe failure (resume pane dies): Tier-1 returns Dead fast
///     (`pane_dead`/`!exists` is unambiguous), then `kill_clean` (~100ms
///     macOS grace) and a typed 409 response preserving the sid.
///
/// HTTP clients should budget ~3-4s worst-case for the resume probe and
/// configure timeouts accordingly.
pub async fn ensure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    // Serialize concurrent ensure calls for the same session. The decision
    // phase reads tmux state and the restart phase mutates it; any other
    // ensure for this id must wait so both see a consistent view.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    // Inspect tmux + make the restart decision on a blocking thread. Refresh
    // the cache first so rapid re-calls see the true current state (the
    // background status poller only refreshes every 2s).
    let decision_instance = instance.clone();
    let id_for_log = id.clone();
    let decision = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        crate::tmux::refresh_session_cache();
        let tmux_session = decision_instance.tmux_session()?;
        let exists = tmux_session.exists();
        let pane_dead = exists && tmux_session.is_pane_dead();
        let needs_restart = if !exists || pane_dead {
            true
        } else if crate::hooks::read_hook_status(&decision_instance.id).is_some() {
            // Hook status tracks this session; shell detection is unreliable.
            false
        } else if decision_instance.has_command_override() {
            // Custom command overrides run agents through wrapper scripts that
            // look like shells to tmux. Don't restart based on shell detection.
            false
        } else {
            !decision_instance.expects_shell() && tmux_session.is_pane_running_shell()
        };
        tracing::debug!(target: "http.api.sessions",
            session_id = id_for_log,
            exists,
            pane_dead,
            needs_restart,
            "ensure_session: restart decision"
        );
        Ok(needs_restart)
    })
    .await;

    let needs_restart = match decision {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "ensure_session: failed to inspect tmux for {id}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "ensure_session inspect panicked for {id}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response();
        }
    };

    if !needs_restart {
        return (StatusCode::OK, Json(serde_json::json!({"status": "alive"}))).into_response();
    }

    if state.read_only {
        // Read-only viewers must not kill + respawn a dead session. Signal
        // the frontend so it can show "session is stopped; ask an owner to
        // reattach" instead of silently replacing the agent process.
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Session is stopped or errored. Restart requires write access.",
            })),
        )
            .into_response();
    }

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.status = crate::session::Status::Starting;
            inst.last_error = None;
        }
    }

    let sync_base = instance.clone();
    let restart_result = tokio::task::spawn_blocking(
        move || -> Result<(Instance, crate::session::StartOutcome), Box<(Instance, anyhow::Error)>> {
            let mut inst = instance;
            // `ensure_session` respawns on demand before a WS attach/send,
            // the server-side analog of `ensure_pane_ready`: always `Allow`,
            // ignoring `auto_resume_on_restart`, so attaching does not drop
            // the agent's context. The instance-level cascade holds the
            // lifecycle lock across final poller drain, exact-pane OMP
            // capture, kill, and relaunch.
            match inst.restart_with_resume_policy(
                None,
                false,
                crate::session::ResumeAttemptPolicy::Allow,
            ) {
                Ok(outcome) => Ok((inst, outcome)),
                Err(e) => Err(Box::new((inst, e))),
            }
        },
    )
    .await;

    match restart_result {
        Ok(Ok((started, outcome))) => {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(inst, &sync_base, &started);
            }
            let resume_outcome = match &outcome {
                crate::session::StartOutcome::Resumed => "resumed",
                crate::session::StartOutcome::ResumeFailed { .. } => "resume_failed",
                crate::session::StartOutcome::Fresh => "fresh",
                crate::session::StartOutcome::FreshAfterFailedResume { .. } => {
                    "fresh_after_failed_resume"
                }
            };
            let mut body = serde_json::json!({
                "status": "restarted",
                "resume_outcome": resume_outcome,
            });
            if let crate::session::StartOutcome::ResumeFailed { sid } = &outcome {
                body["status"] = serde_json::Value::String("resume_failed".to_string());
                body["error"] = serde_json::Value::String("resume_failed".to_string());
                body["message"] = serde_json::Value::String(format!(
                    "Resume failed for sid {sid}; preserved for explicit retry"
                ));
                body["resume_session_id"] = serde_json::Value::String(sid.clone());
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            if let crate::session::StartOutcome::FreshAfterFailedResume { sid } = &outcome {
                body["message"] = serde_json::Value::String(format!(
                    "Started fresh; a prior resume attempt failed for sid {sid}. \
                     The old conversation is still reachable via the agent's own \
                     resume/history picker."
                ));
                body["prior_session_id"] = serde_json::Value::String(sid.clone());
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "ensure_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                if apply_post_restart_sync(inst, &sync_base, &started) {
                    inst.status = crate::session::Status::Error;
                    inst.last_error = Some(msg.clone());
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "restart_failed",
                    "message": msg,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "ensure_session panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

// --- Paired terminal ---

pub async fn ensure_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let index = q.index;
    if index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    // Serialize concurrent terminal-ensure calls for the same session so two
    // parallel requests don't both try to create the same tmux session
    // (the second would fail with "duplicate session"). Taken before the
    // snapshot read so a concurrent mutation cannot land between the two and
    // hand `spawn_blocking` a stale clone.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let inst = match instances.iter().find(|i| i.id == id) {
        Some(i) => i.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
    };
    drop(instances);

    // Index 0 has the in-memory `terminal_info.created` fast path; additional
    // terminals (index >= 1) are queried straight from tmux. Either way the
    // pane shell can exit (Ctrl+D, `exit`, SIGHUP from a destroyed tmux client,
    // etc.) while the session keeps existing (we set `remain-on-exit on`), so a
    // live-but-dead pane must be respawned the same way the TUI does on attach.
    {
        let session = inst.terminal_tmux_session_indexed(index).ok();
        let known = if index == 0 {
            inst.has_terminal()
        } else {
            session.as_ref().map(|s| s.exists()).unwrap_or(false)
        };
        if known {
            let pane_dead = session
                .map(|s| s.exists() && s.is_pane_dead())
                .unwrap_or(false);
            if !pane_dead {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "exists"})),
                )
                    .into_response();
            }
            tracing::warn!(
                target: "terminal.ws",
                session = %id,
                index,
                "paired terminal pane is dead, respawning"
            );
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_terminal_if_dead_indexed(index);
        inst_clone.start_terminal_with_size_indexed(index, None)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            // Only index 0 carries an in-memory cache flag.
            if index == 0 {
                let mut instances = state.instances.write().await;
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
                }
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({"status": "created"})),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Terminal creation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "create_failed", "message": "Failed to create terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Terminal creation panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn ensure_container_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let index = q.index;
    if index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    // Lock-then-read, matching `ensure_terminal`.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let inst = match instances.iter().find(|i| i.id == id) {
        Some(i) => i.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
    };
    drop(instances);

    // Same dead-pane rescue as `ensure_terminal`: an existing-but-dead
    // pane would otherwise silently swallow every keystroke from the
    // browser. Container terminals are always tmux-queried (no cache flag).
    {
        let session = inst.container_terminal_tmux_session_indexed(index).ok();
        if session.as_ref().map(|s| s.exists()).unwrap_or(false) {
            let pane_dead = session
                .map(|s| s.exists() && s.is_pane_dead())
                .unwrap_or(false);
            if !pane_dead {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "exists"})),
                )
                    .into_response();
            }
            tracing::warn!(
                target: "terminal.ws",
                session = %id,
                index,
                "container terminal pane is dead, respawning"
            );
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_container_terminal_if_dead_indexed(index);
        inst_clone.start_container_terminal_with_size_indexed(index, None)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "created"})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Container terminal creation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "create_failed", "message": "Failed to create container terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Container terminal creation panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// Kill an additional paired terminal (host + container) at `index`. Used when
/// the web dashboard closes an extra terminal tab so its tmux shell does not
/// leak for the session's lifetime. Index 0 is the primary terminal shared with
/// the native TUI; closing it in the web UI only hides the pane (the TUI keeps
/// its shell), so this endpoint rejects index 0. See #2437.
pub async fn kill_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let index = q.index;
    if index == 0 || index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    // Lock-then-read, matching `ensure_terminal`.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let inst = match instances.iter().find(|i| i.id == id) {
        Some(i) => i.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
    };
    drop(instances);

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // A missing session is success (the `kill_*` helpers no-op when the
        // tmux session is absent); only a real tmux failure surfaces here, so
        // the caller can retry instead of leaving an orphaned shell behind.
        inst.kill_terminal_indexed(index)?;
        inst.kill_container_terminal_indexed(index)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "killed"})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Terminal kill failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "kill_failed", "message": "Failed to kill terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Terminal kill panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

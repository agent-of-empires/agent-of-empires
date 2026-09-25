//! Group/notification/diff-base updates and the shared persist helper.

use super::*;

pub(super) fn apply_session_group(inst: &mut Instance, group: String) {
    inst.group_path = group;
}

/// `PATCH /api/sessions/:id/group`. Moves an existing session to another
/// group, creates a new group by assigning its path, or clears the group
/// (empty string). Web parity with the TUI rename dialog and `aoe session
/// rename --group`, which already support post-create group edits.
pub async fn update_session_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateGroupBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let group = body.group;
    // Match `create_session`'s group handling exactly: display-label
    // check on a non-empty path, no trimming or slash normalization. The
    // empty string is the ungroup sentinel and skips validation.
    if !group.is_empty() {
        if let Err(msg) = validate_display_label(&group, "group") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": msg })),
            )
                .into_response();
        }
    }

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "group update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_session_group(inst, group);
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Return the mutation result and its complete committed profile bundle.
/// Callers must leave the mirror and side effects untouched on failure.
pub(crate) async fn persist_session_update<F, R>(
    profile: String,
    label: &'static str,
    file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    mutate: F,
) -> Result<(R, Vec<Instance>, Vec<crate::session::Group>), ()>
where
    F: FnOnce(&mut Vec<Instance>) -> R + Send + 'static,
    R: Send + 'static,
{
    let storage = match Storage::open(&profile, file_watch) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "http.api.sessions",
                "Failed to open storage for {label}: {e}"
            );
            return Err(());
        }
    };
    match tokio::task::spawn_blocking(move || {
        storage.update_with_snapshot(|instances, _groups| Ok(mutate(instances)))
    })
    .await
    {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => {
            tracing::error!(
                target: "http.api.sessions",
                "Failed to persist {label}: {e}"
            );
            Err(())
        }
        Err(e) => {
            tracing::error!(
                target: "http.api.sessions",
                "Persist join failed for {label}: {e}"
            );
            Err(())
        }
    }
}

pub(super) async fn commit_profile_update<F, R>(
    state: &Arc<AppState>,
    profile: String,
    label: &'static str,
    mutate: F,
    status_id: Option<String>,
) -> Result<R, axum::response::Response>
where
    F: FnOnce(&mut Vec<Instance>) -> R + Send + 'static,
    R: Send + 'static,
{
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let commit_state = state.clone();
    let commit_profile = profile.clone();
    let persisted = tokio::task::spawn_blocking(move || {
        let store = crate::server::session_store::NativeSessionStore::open(
            commit_state,
            &commit_profile,
            status_id,
        )?;
        (&store as &dyn crate::session::SessionStore).update(|rows, _| Ok(mutate(rows)))
    })
    .await;
    match persisted {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) if error.is::<crate::session::NativeStoreUnavailable>() => {
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", %error, %label, "session commit failed");
            Err(persist_failed_response())
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", %error, %label, "session commit task failed");
            state
                .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles: vec![profile],
                })
                .await;
            Err(persist_failed_response())
        }
    }
}

pub(super) async fn adopt_profile_update<R>(
    state: &Arc<AppState>,
    profile: String,
    result: Result<(R, Vec<Instance>, Vec<crate::session::Group>), ()>,
    committed_status: impl Fn(&str) -> bool,
    publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) -> Result<R, axum::response::Response> {
    let committed = match result {
        Ok((result, rows, groups)) => crate::server::reload::adopt_committed_profiles(
            state,
            [(&profile, rows, groups)],
            committed_status,
            publication,
        )
        .await
        .map(|()| result),
        Err(()) => Err(crate::server::reload::ReloadFailure {
            health: crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::ProfileData,
                profiles: vec![profile],
            },
            source: anyhow::anyhow!("session commit failed"),
        }),
    };
    match committed {
        Ok(result) => Ok(result),
        Err(error) => {
            *state.canonical_health.write().await = error.health;
            state.runtime.request_publish();
            Err(persist_failed_response())
        }
    }
}

/// Report a storage failure without exposing host diagnostics.
pub(super) fn persist_failed_response() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "persist_failed",
            "message": "Failed to persist session update"
        })),
    )
        .into_response()
}

pub async fn update_session_notifications(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateNotificationsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Apply each field independently. `Unset` leaves the stored value
    // alone; `Clear` sets it to None (inherit default); `Set(v)` writes
    // an explicit override.
    fn apply(target: &mut Option<bool>, tri: Tristate) {
        match tri {
            Tristate::Unset => {}
            Tristate::Clear => *target = None,
            Tristate::Set(v) => *target = Some(v),
        }
    }

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    let waiting = body.notify_on_waiting;
    let idle = body.notify_on_idle;
    let error = body.notify_on_error;

    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "notification update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply(&mut inst.notify_on_waiting, waiting);
                apply(&mut inst.notify_on_idle, idle);
                apply(&mut inst.notify_on_error, error);
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Apply an override to the selected repo or the single-repo checkout.
pub(super) fn apply_diff_base_override(
    inst: &mut crate::session::Instance,
    repo: Option<&str>,
    value: Option<String>,
) {
    match repo {
        Some(name) => {
            if let Some(ws) = inst.workspace_info.as_mut() {
                if let Some(r) = ws.repos.iter_mut().find(|r| r.name == name) {
                    r.base_branch_override = value;
                }
            }
        }
        None => inst.base_branch_override = value,
    }
}

pub async fn update_session_diff_base(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateDiffBaseBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        // Reject a target that names no entry, so a stale client cannot
        // silently write an override the diff never reads.
        match body.repo.as_deref() {
            Some(name) => {
                if !inst.all_repos().iter().any(|r| r.name == name) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "bad_request",
                            "message": "unknown workspace repo"
                        })),
                    )
                        .into_response();
                }
            }
            None => {
                if inst.workspace_info.is_some() {
                    let names: Vec<&str> =
                        inst.all_repos().iter().map(|r| r.name.as_str()).collect();
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "bad_request",
                            "message": format!(
                                "this session is a multi-repo workspace; name the repo to set a diff base for ({})",
                                names.join(", ")
                            )
                        })),
                    )
                        .into_response();
                }
            }
        }
        inst.source_profile.clone()
    };

    let new_override = body
        .base_branch
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);

    let persist_id = id.clone();
    let persist_repo = body.repo;
    let committed = commit_profile_update(
        &state,
        profile,
        "diff-base update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_diff_base_override(inst, persist_repo.as_deref(), new_override);
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

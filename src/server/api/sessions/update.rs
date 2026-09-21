//! Group/notification/diff-base updates and the shared persist helper.

use super::*;

// --- Update session group ---

#[derive(Deserialize)]
pub struct UpdateGroupBody {
    /// Destination group path; the empty string means ungrouped. A non-empty
    /// path auto-creates the group, since `/api/groups` and the `GroupTree`
    /// render model both derive groups from instance `group_path` values.
    pub group: String,
}

pub(super) fn apply_session_group(inst: &mut Instance, group: String) {
    inst.group_path = group;
}

/// `PATCH /api/sessions/:id/group`. Moves a session to another group, creates
/// one by assigning its path, or clears it with the empty string. Web parity
/// with the TUI rename dialog and `aoe session rename --group`.
///
/// Persist-first like the other per-field PATCH sub-routes, so a failed write
/// returns 500 without leaving memory and disk diverged (#1589).
pub async fn update_session_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateGroupBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let group = body.group;
    // Match `create_session`'s group handling exactly: display-label check on a
    // non-empty path, no trimming or slash normalization. The empty string is
    // the ungroup sentinel and skips validation.
    if !group.is_empty() {
        if let Err(msg) = validate_display_label(&group, "group") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": msg })),
            )
                .into_response();
        }
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        inst.source_profile.clone()
    };

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_group = group.clone();
    if persist_session_update(
        profile,
        "group update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_session_group(inst, persist_group);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "group update: instance vanished after persist"
        );
        return crate::server::api::session_gone_after_persist();
    };
    apply_session_group(inst, group);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Update session notification preferences ---

/// Body for `PATCH /api/sessions/:id/notifications`. Each field is an outer
/// Option so absence means "leave alone", with an inner Option where
/// `Some(null)` means "clear this override".
#[derive(Deserialize, Default)]
pub struct UpdateNotificationsBody {
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_waiting: Tristate,
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_idle: Tristate,
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_error: Tristate,
}

/// Three-state field representing JSON `undefined | null | true | false`:
/// - Unset: leave the current session value untouched.
/// - Clear: set to None (inherit the server default).
/// - Set(v): explicit user override.
#[derive(Default, Copy, Clone)]
pub enum Tristate {
    #[default]
    Unset,
    Clear,
    Set(bool),
}

fn deserialize_tristate<'de, D>(d: D) -> Result<Tristate, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Option<Option<bool>>: absent -> None, null -> Some(None), bool -> Some(Some(bool))
    let v: Option<Option<bool>> = Option::deserialize(d)?;
    Ok(match v {
        None => Tristate::Unset,
        Some(None) => Tristate::Clear,
        Some(Some(b)) => Tristate::Set(b),
    })
}

/// Persist a session mutation to its profile store before touching memory.
///
/// Runs `mutate` inside the storage `update` transaction on a blocking thread,
/// collapsing store-open, write and join failures into `Err(())` after logging
/// with `label`. Callers MUST treat `Err` as HTTP 500 and leave the in-memory
/// instance untouched: persisting first is what keeps disk and memory in
/// agreement, and stops archive/snooze side effects firing on a write that
/// never landed (#1589).
pub(crate) async fn persist_session_update<F>(
    profile: String,
    label: &'static str,
    file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    mutate: F,
) -> Result<(), ()>
where
    F: FnOnce(&mut Vec<Instance>) + Send + 'static,
{
    let storage = match Storage::new(&profile, file_watch) {
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
        storage.update(|instances, _groups| {
            mutate(instances);
            Ok(())
        })
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
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

/// 500 response for a `persist_session_update` failure. The body shape matches
/// the other JSON errors in this module, so the dashboard's `!res.ok` handling
/// reads the same keys.
pub(super) fn persist_failed_response() -> axum::response::Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "persist_failed",
        "Failed to persist session update",
    )
}

pub async fn update_session_notifications(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateNotificationsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // `Unset` leaves the stored value alone, `Clear` sets it to None (inherit
    // default), `Set(v)` writes an explicit override.
    fn apply(target: &mut Option<bool>, tri: Tristate) {
        match tri {
            Tristate::Unset => {}
            Tristate::Clear => *target = None,
            Tristate::Set(v) => *target = Some(v),
        }
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        inst.source_profile.clone()
    };

    let waiting = body.notify_on_waiting;
    let idle = body.notify_on_idle;
    let error = body.notify_on_error;

    // Persist first; only mutate memory once disk is durable (#1589).
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "notification update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply(&mut inst.notify_on_waiting, waiting);
                apply(&mut inst.notify_on_idle, idle);
                apply(&mut inst.notify_on_error, error);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "notification update: instance vanished after persist"
        );
        return crate::server::api::session_gone_after_persist();
    };
    apply(&mut inst.notify_on_waiting, waiting);
    apply(&mut inst.notify_on_idle, idle);
    apply(&mut inst.notify_on_error, error);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// `PATCH /api/sessions/{id}/diff-base` sets or clears the diff base override,
// scoped to one repo. The web `vs <ref>` chip, the TUI diff view's `b` keybind,
// and `aoe session set-base` all funnel through this endpoint or its storage
// equivalent, so the override survives restart. A workspace session must name
// the repo; a single-repo session omits it (#970, #3329).

#[derive(Deserialize)]
pub struct UpdateDiffBaseBody {
    /// New override. `Some(non-empty)` sets it; `Some("")` or `None` clears it,
    /// falling back to the recorded creation base, the profile default, then
    /// auto-detection.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Workspace repo this override applies to. Omitting it targets the
    /// session's own checkout, which only a single-repo session has; omitting it
    /// on a workspace is rejected rather than writing state nothing reads.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Write a diff-base override onto the entry `repo` names, or onto the
/// session's own checkout when it is `None`. Split out so the persist closure
/// and the in-memory update cannot drift.
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
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        // Reject a target that names no entry, so a stale client cannot write
        // an override the diff never reads.
        match body.repo.as_deref() {
            Some(name) => {
                if !inst.all_repos().iter().any(|r| r.name == name) {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "bad_request",
                        "unknown workspace repo",
                    );
                }
            }
            None => {
                if inst.workspace_info.is_some() {
                    let names: Vec<&str> =
                        inst.all_repos().iter().map(|r| r.name.as_str()).collect();
                    return api_error(StatusCode::BAD_REQUEST, "bad_request", format!(
                                "this session is a multi-repo workspace; name the repo to set a diff base for ({})",
                                names.join(", ")
                            ));
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

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_override = new_override.clone();
    let persist_repo = body.repo.clone();
    if persist_session_update(
        profile,
        "diff-base update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_diff_base_override(inst, persist_repo.as_deref(), persist_override);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "diff-base update: instance vanished after persist"
        );
        return crate::server::api::session_gone_after_persist();
    };
    apply_diff_base_override(inst, body.repo.as_deref(), new_override);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// Three sibling endpoints surface `Instance::pin`, `archive` and `snooze` to
// the dashboard, all read-only-403 then persist-then-mutate. Archive also tears
// down the tmux pane and, for structured sessions, the worker. Mutual-exclusion
// invariants live in the `Instance` methods, so the handlers never set fields
// directly (#1581).

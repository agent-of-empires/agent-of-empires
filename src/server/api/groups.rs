//! Scoped group mutations with complete profile publication.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::AppState;
use crate::daemon::{ApiErrorCode, CollapseGroupBody, GroupLocation, MoveGroupBody, RuntimeHealth};
use crate::server::session_store::NativeSessionStore;
use crate::session::{GroupMovePlan, GroupTree, Instance, SessionStore, Status};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct GroupRejected {
    status: StatusCode,
    message: &'static str,
    code: Option<ApiErrorCode>,
}

fn reject(status: StatusCode, message: &'static str) -> anyhow::Error {
    GroupRejected {
        status,
        message,
        code: None,
    }
    .into()
}

fn reject_with_code(code: ApiErrorCode, message: &'static str) -> anyhow::Error {
    GroupRejected {
        status: code.status(),
        message,
        code: Some(code),
    }
    .into()
}

fn belongs(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn check_structured(row: &Instance) -> Result<()> {
    if !row.is_structured() {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "CityHall requires structured sessions",
        ));
    }
    Ok(())
}

fn check_row(row: &Instance, cityhall: bool) -> Result<()> {
    if cityhall {
        check_structured(row)?;
    }
    if matches!(row.status, Status::Creating | Status::Deleting)
        || row.has_fresh_lifecycle_reservation(crate::time::Timestamp::now())
    {
        return Err(reject(StatusCode::CONFLICT, "Session lifecycle is busy"));
    }
    Ok(())
}

fn check_members(rows: &[Instance], root: &str, ids: &[String], cityhall: bool) -> Result<()> {
    let mut count = 0;
    for row in rows.iter().filter(|row| belongs(&row.group_path, root)) {
        check_row(row, cityhall)?;
        if ids.binary_search(&row.id).is_err() {
            return Err(reject(StatusCode::CONFLICT, "Group membership changed"));
        }
        count += 1;
    }
    if count != ids.len() {
        return Err(reject(StatusCode::CONFLICT, "Group membership changed"));
    }
    Ok(())
}

async fn failure(state: &AppState, error: anyhow::Error) -> Response {
    if let Some(rejected) = error.downcast_ref::<GroupRejected>() {
        let mut response = (
            rejected.status,
            Json(serde_json::json!({"message": rejected.message})),
        )
            .into_response();
        if let Some([(name, value)]) = rejected.code.map(ApiErrorCode::header) {
            response
                .headers_mut()
                .insert(name, axum::http::HeaderValue::from_static(value));
        }
        return response;
    }
    if error.is::<crate::session::ProfileMoveRejected>() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"message": error.to_string()})),
        )
            .into_response();
    }
    tracing::warn!(target: "http.api.groups", %error, "group operation failed");
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

pub async fn move_group(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MoveGroupBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if state.cityhall_mode && body.source.profile != body.target.profile {
        return super::cityhall_response();
    }
    for location in [&body.source, &body.target] {
        if let Err(response) = validate_location(location) {
            return response;
        }
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    {
        let metadata = state.canonical_metadata.read().await;
        for location in [&body.source, &body.target] {
            if !metadata
                .profiles
                .iter()
                .any(|profile| profile.name == location.profile)
            {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "profile_not_found", "message": "Profile not found"}))).into_response();
            }
        }
    }
    let worker_state = state.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
        let source = NativeSessionStore::open(worker_state.clone(), &body.source.profile, None)?;
        let target = if body.source.profile == body.target.profile {
            None
        } else {
            Some(NativeSessionStore::open(
                worker_state.clone(),
                &body.target.profile,
                None,
            )?)
        };
        let mut ids = source
            .load()?
            .into_iter()
            .filter(|row| belongs(&row.group_path, &body.source.path))
            .map(|row| {
                check_row(&row, worker_state.cityhall_mode)?;
                Ok(row.id)
            })
            .collect::<Result<Vec<_>>>()?;
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(reject(
                StatusCode::CONFLICT,
                "Group contains duplicate session identities",
            ));
        }
        Ok((body, source, target, ids))
    })
    .await;
    let (body, source, target, ids) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => return failure(&state, error).await,
        Err(error) => return failure(&state, error.into()).await,
    };
    let (profile, target_profile) = (body.source.profile.clone(), body.target.profile.clone());
    let (from, to) = (body.source.path.clone(), body.target.path.clone());
    let mut submission_guards = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(guard) = state
            .session_service
            .prompt_submission_for_session(id)
            .await
        {
            submission_guards.push(guard);
        }
    }
    let mut instance_guards = Vec::with_capacity(ids.len());
    for id in &ids {
        instance_guards.push(state.instance_lock(id).await.lock_owned().await);
    }
    let cityhall = state.cityhall_mode;
    let committed = tokio::task::spawn_blocking(move || -> Result<()> {
        let _identity = crate::session::acquire_session_identity_lock()?;
        source.check_available()?;
        let mut lifecycle_guards = Vec::with_capacity(ids.len());
        for id in &ids {
            lifecycle_guards.push(source.storage().acquire_instance_lifecycle_lock(id)?);
        }
        if let Some(target) = target {
            let rows = source.load()?;
            check_members(&rows, &body.source.path, &ids, cityhall)?;
            let changes: Vec<_> = rows
                .into_iter()
                .filter(|row| belongs(&row.group_path, &body.source.path))
                .map(|before| {
                    let mut after = before.clone();
                    after.group_path = format!(
                        "{}{}",
                        body.target.path,
                        &before.group_path[body.source.path.len()..]
                    );
                    (before, after)
                })
                .collect();
            source.move_instances_to(
                &target,
                &changes,
                &GroupMovePlan::subtree(&body.source.path, &body.target.path),
                |existing, candidates| {
                    for (index, row) in candidates.iter().enumerate() {
                        check_row(row, cityhall)?;
                        if crate::session::is_duplicate_session(
                            existing.iter().chain(candidates[..index].iter()),
                            &row.title,
                            &row.project_path,
                            None,
                        ) {
                            return Err(reject(
                                StatusCode::CONFLICT,
                                "Session already exists with same title and path",
                            ));
                        }
                    }
                    Ok(())
                },
            )?;
        } else {
            (&source as &dyn SessionStore).update(|rows, groups| {
                check_members(rows, &body.source.path, &ids, cityhall)?;
                let mut tree = GroupTree::new_with_groups(rows, groups);
                if !tree.group_exists(&body.source.path) {
                    return Err(reject(
                        StatusCode::NOT_FOUND,
                        "Source group no longer exists",
                    ));
                }
                if body.source.path != body.target.path && tree.group_exists(&body.target.path) {
                    return Err(reject(StatusCode::CONFLICT, "Target group already exists"));
                }
                tree.rename_group(&body.source.path, &body.target.path);
                for row in rows
                    .iter_mut()
                    .filter(|row| belongs(&row.group_path, &body.source.path))
                {
                    row.group_path = format!(
                        "{}{}",
                        body.target.path,
                        &row.group_path[body.source.path.len()..]
                    );
                }
                *groups = tree.get_all_groups();
                Ok(())
            })?;
        }
        Ok(())
    })
    .await;
    drop(instance_guards);
    drop(submission_guards);
    drop(namespace);
    match committed {
        Ok(Ok(())) => {
            tracing::info!(target: "session.sync", %profile, %target_profile, %from, %to, "group moved");
            match state.runtime.publish(&state).await {
                Ok(snapshot) => crate::server::runtime::mutation_response(
                    &snapshot.value.cursor,
                    Json(serde_json::json!({"ok": true})),
                ),
                Err(error) => failure(&state, error).await,
            }
        }
        Ok(Err(error)) => failure(&state, error).await,
        Err(error) => failure(&state, error.into()).await,
    }
}

fn validate_location(location: &GroupLocation) -> Result<(), Response> {
    if location.path.is_empty() || crate::session::is_synthetic_project_header(&location.path) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message": "An explicit persisted group path is required"})),
        )
            .into_response());
    }
    super::validate_display_label(&location.path, "group").map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message": message})),
        )
            .into_response()
    })
}

enum GroupMetadataChange {
    Create,
    Collapse(bool),
}

async fn commit_group_metadata(
    state: Arc<AppState>,
    group: GroupLocation,
    change: GroupMetadataChange,
) -> Response {
    if let Err(response) = validate_location(&group) {
        return response;
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !state
        .canonical_metadata
        .read()
        .await
        .profiles
        .iter()
        .any(|profile| profile.name == group.profile)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "profile_not_found", "message": "Profile not found"})),
        )
            .into_response();
    }
    let (status, op) = match &change {
        GroupMetadataChange::Create => (StatusCode::CREATED, "group created"),
        GroupMetadataChange::Collapse(_) => (StatusCode::OK, "group collapsed"),
    };
    let (profile, path) = (group.profile.clone(), group.path.clone());
    let worker_state = state.clone();
    let committed = tokio::task::spawn_blocking(move || -> Result<()> {
        let store = NativeSessionStore::open(worker_state.clone(), &group.profile, None)?;
        (&store as &dyn SessionStore).update(|rows, groups| {
            if worker_state.cityhall_mode
                && rows
                    .iter()
                    .any(|row| belongs(&row.group_path, &group.path) && !row.is_structured())
            {
                return Err(reject(
                    StatusCode::FORBIDDEN,
                    "CityHall requires structured sessions",
                ));
            }
            let mut tree = GroupTree::new_with_groups(rows, groups);
            match change {
                GroupMetadataChange::Create => {
                    if tree.group_exists(&group.path) {
                        return Err(reject(StatusCode::CONFLICT, "Group already exists"));
                    }
                    tree.create_group(&group.path);
                }
                GroupMetadataChange::Collapse(collapsed) => {
                    if !tree.group_exists(&group.path) {
                        return Err(reject(StatusCode::NOT_FOUND, "Group no longer exists"));
                    }
                    tree.set_collapsed(&group.path, collapsed);
                }
            }
            *groups = tree.get_all_groups();
            Ok(())
        })
    })
    .await;
    drop(namespace);
    match committed {
        Ok(Ok(())) => {
            tracing::info!(target: "session.sync", %profile, group = %path, "{op}");
            match state.runtime.publish(&state).await {
                Ok(snapshot) => crate::server::runtime::mutation_response(
                    &snapshot.value.cursor,
                    (status, Json(serde_json::json!({"ok": true}))),
                ),
                Err(error) => failure(&state, error).await,
            }
        }
        Ok(Err(error)) => failure(&state, error).await,
        Err(error) => failure(&state, error.into()).await,
    }
}

pub async fn create_group(
    State(state): State<Arc<AppState>>,
    body: Result<Json<GroupLocation>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(group) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    commit_group_metadata(state, group, GroupMetadataChange::Create).await
}

pub async fn collapse_group(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CollapseGroupBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    commit_group_metadata(
        state,
        body.group,
        GroupMetadataChange::Collapse(body.collapsed),
    )
    .await
}

pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    body: Result<Json<crate::daemon::DeleteGroupBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if let Err(response) = validate_location(&body.group) {
        return response;
    }
    if (!matches!(body.mode, crate::daemon::DeleteGroupMode::DeleteSessions)
        && (body.cleanup.delete_worktree
            || body.cleanup.delete_branch
            || body.cleanup.delete_sandbox
            || body.cleanup.force_delete
            || body.cleanup.keep_scratch
            || body.cleanup.expected_trash
            || body.cleanup.use_cleanup_defaults))
        || body.cleanup.expected_trash
        || body.cleanup.use_cleanup_defaults
        || body.cleanup.keep_scratch
    {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "invalid_cleanup_mode", "message": "Group deletion requires explicit cleanup options; cleanup is valid only with delete_sessions"
        }))).into_response();
    }
    match tokio::spawn(delete_group_owned(state, body)).await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn delete_group_owned(
    state: Arc<AppState>,
    body: crate::daemon::DeleteGroupBody,
) -> Response {
    use crate::daemon::{
        DeleteGroupMode, DeleteGroupOutcome, GroupSessionFailure, GroupSessionFailureCode,
        GroupSessionOutcome, PurgeOutcome,
    };
    let namespace = state
        .runtime
        .purge_namespace_lease(&state.profile_namespace)
        .await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !state
        .canonical_metadata
        .read()
        .await
        .profiles
        .iter()
        .any(|profile| profile.name == body.group.profile)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "profile_not_found", "message": "Profile not found"})),
        )
            .into_response();
    }
    let worker_state = state.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
        let store = NativeSessionStore::open(worker_state.clone(), &body.group.profile, None)?;
        let mut ids = store
            .load()?
            .into_iter()
            .filter(|row| belongs(&row.group_path, &body.group.path))
            .map(|row| {
                check_row(&row, worker_state.cityhall_mode)?;
                Ok(row.id)
            })
            .collect::<Result<Vec<_>>>()?;
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(reject(
                StatusCode::CONFLICT,
                "Group contains duplicate session identities",
            ));
        }
        Ok((store, body, ids))
    })
    .await;
    let (store, body, ids) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => return failure(&state, error).await,
        Err(error) => return failure(&state, error.into()).await,
    };
    let mut submission_guards = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(guard) = state
            .session_service
            .prompt_submission_for_session(id)
            .await
        {
            submission_guards.push(guard);
        }
    }
    let mut instance_guards = Vec::with_capacity(ids.len());
    for id in &ids {
        instance_guards.push(state.instance_lock(id).await.lock_owned().await);
    }
    let cityhall = state.cityhall_mode;
    let committed = tokio::task::spawn_blocking(move || -> Result<_> {
        let _identity = crate::session::acquire_session_identity_lock()?;
        store.check_available()?;
        let mut lifecycle_guards = Vec::with_capacity(ids.len());
        for id in &ids {
            lifecycle_guards.push(store.storage().acquire_instance_lifecycle_lock(id)?);
        }
        let mut detached = Vec::with_capacity(ids.len());
        (&store as &dyn SessionStore).update(|rows, groups| {
            check_members(rows, &body.group.path, &ids, cityhall)?;
            let mut tree = GroupTree::new_with_groups(rows, groups);
            if !tree.group_exists(&body.group.path) {
                return Err(reject(StatusCode::NOT_FOUND, "Group no longer exists"));
            }
            if matches!(body.mode, DeleteGroupMode::EmptyOnly) && !ids.is_empty() {
                return Err(reject_with_code(
                    ApiErrorCode::GroupNotEmpty,
                    "Group is not empty",
                ));
            }
            tree.delete_group(&body.group.path);
            for row in rows
                .iter_mut()
                .filter(|row| belongs(&row.group_path, &body.group.path))
            {
                row.group_path.clear();
                if matches!(body.mode, DeleteGroupMode::MoveToParent) {
                    row.group_path.push_str(
                        body.group
                            .path
                            .rsplit_once('/')
                            .map_or("", |(parent, _)| parent),
                    );
                }
                detached.push(row.clone());
            }
            *groups = tree.get_all_groups();
            Ok(())
        })?;
        detached.sort_by(|left, right| left.id.cmp(&right.id));
        Ok((body, detached))
    })
    .await;
    let (body, detached) = match committed {
        Ok(Ok(committed)) => committed,
        Ok(Err(error)) => return failure(&state, error).await,
        Err(error) => return failure(&state, error.into()).await,
    };
    let mut outcome = DeleteGroupOutcome {
        sessions: Vec::with_capacity(detached.len()),
        failures: Vec::new(),
        group_removed: true,
    };
    for row in detached {
        let id = row.id.clone();
        if !matches!(body.mode, DeleteGroupMode::DeleteSessions) {
            outcome.sessions.push(GroupSessionOutcome {
                id,
                outcome: PurgeOutcome::Kept {
                    messages: Vec::new(),
                    teardown_started: false,
                },
            });
            continue;
        }
        let managed = row.has_managed_worktree_or_workspace();
        let cleanup = crate::daemon::DeleteSessionBody {
            delete_worktree: body.cleanup.delete_worktree && managed,
            delete_branch: body.cleanup.delete_branch && managed,
            delete_sandbox: body.cleanup.delete_sandbox
                && row
                    .sandbox_info
                    .as_ref()
                    .is_some_and(|sandbox| sandbox.enabled),
            force_delete: body.cleanup.force_delete,
            keep_scratch: false,
            ..Default::default()
        };
        // Capture POST-detachment location and generation, not the removed group's path.
        let selection = crate::session::deletion::PurgeSelection {
            profile: row.source_profile.clone(),
            group_path: row.group_path.clone(),
            lifecycle_generation: row.lifecycle_generation,
        };
        let recent = crate::session::recent_project_entry_for(&row);
        match super::sessions::purge_session_artifacts(
            &state,
            &id,
            row,
            &cleanup,
            recent,
            None,
            Some(selection),
        )
        .await
        {
            Ok(purge) => outcome
                .sessions
                .push(GroupSessionOutcome { id, outcome: purge }),
            Err(error) => {
                if error.is::<crate::session::NativeStoreUnavailable>()
                    || *state.canonical_health.read().await != RuntimeHealth::Healthy
                {
                    return failure(&state, error).await;
                }
                let code = match error.downcast_ref::<crate::session::LifecycleReservationError>() {
                    Some(crate::session::LifecycleReservationError::Busy(_)) => {
                        GroupSessionFailureCode::LifecycleBusy
                    }
                    Some(crate::session::LifecycleReservationError::Superseded) => {
                        GroupSessionFailureCode::Superseded
                    }
                    _ => GroupSessionFailureCode::PurgeFailed,
                };
                tracing::warn!(target: "http.api.groups", session = %id, %error, "detached member purge failed");
                let message = match code {
                    GroupSessionFailureCode::LifecycleBusy => "Session lifecycle is busy",
                    GroupSessionFailureCode::Superseded => "Session changed before purge",
                    GroupSessionFailureCode::PurgeFailed => {
                        "Session purge failed; detached session was retained"
                    }
                }
                .to_owned();
                outcome
                    .failures
                    .push(GroupSessionFailure { id, code, message });
            }
        }
    }
    drop(instance_guards);
    drop(submission_guards);
    drop(namespace);
    tracing::info!(
        target: "session.sync",
        group = ?body.group,
        mode = ?body.mode,
        purged = outcome
            .sessions
            .iter()
            .filter(|session| matches!(session.outcome, PurgeOutcome::Deleted { .. }))
            .count(),
        failed = outcome.failures.len(),
        "group deleted"
    );
    match state.runtime.publish(&state).await {
        Ok(snapshot) => {
            crate::server::runtime::mutation_response(&snapshot.value.cursor, Json(outcome))
        }
        Err(error) => failure(&state, error).await,
    }
}

pub async fn reorder(
    State(state): State<Arc<AppState>>,
    body: Result<Json<crate::daemon::ReorderBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if let crate::daemon::ReorderBody::Session {
        freeze: Some(freeze),
        ..
    }
    | crate::daemon::ReorderBody::Group {
        freeze: Some(freeze),
        ..
    } = &body
    {
        if let Some(refused) = super::sessions::oversized_order(&freeze.order) {
            return refused;
        }
    }
    match &body {
        crate::daemon::ReorderBody::Group { group, .. } => {
            if let Err(response) = validate_location(group) {
                return response;
            }
        }
        crate::daemon::ReorderBody::Session {
            source_group,
            destination,
            ..
        } => {
            for path in std::iter::once(source_group).chain(destination) {
                if !path.is_empty()
                    && (crate::session::is_synthetic_project_header(path)
                        || super::validate_display_label(path, "group").is_err())
                {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "message": "A persisted group path or ungrouped bucket is required"
                        })),
                    )
                        .into_response();
                }
            }
        }
    }
    // An admitted command survives HTTP cancellation with its namespace exclusion.
    match tokio::spawn(reorder_owned(state, body)).await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn reorder_owned(state: Arc<AppState>, body: crate::daemon::ReorderBody) -> Response {
    use crate::daemon::{ReorderBody, ReorderOutcome};
    // A metadata-only transition may touch the entire live sibling set. Taking the
    // namespace write guard avoids selecting a partial set of per-session locks.
    let namespace = state.profile_namespace.write().await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let profile = match &body {
        ReorderBody::Session { profile, .. } => profile,
        ReorderBody::Group { group, .. } => &group.profile,
    };
    let exists = state
        .canonical_metadata
        .read()
        .await
        .profiles
        .iter()
        .any(|entry| &entry.name == profile);
    let reordered_group = match &body {
        ReorderBody::Group {
            group, direction, ..
        } => Some((
            group.profile.clone(),
            group.path.clone(),
            format!("{direction:?}"),
        )),
        ReorderBody::Session { .. } => None,
    };
    let result = if !exists {
        Ok(ReorderOutcome::Stale)
    } else {
        let worker_state = state.clone();
        let profile = profile.clone();
        match tokio::task::spawn_blocking(move || -> Result<ReorderOutcome> {
            let _identity = crate::session::acquire_session_identity_lock()?;
            let store = NativeSessionStore::open(worker_state.clone(), &profile, None)?;
            let _lifecycle = match &body {
                ReorderBody::Session { id, .. } => {
                    Some(store.storage().acquire_instance_lifecycle_lock(id)?)
                }
                ReorderBody::Group { .. } => None,
            };
            let mut outcome = ReorderOutcome::Stale;
            let mut replaced_order = None;
            let committed = (&store as &dyn SessionStore).update(|rows, groups| {
                let freeze = match &body {
                    ReorderBody::Session { freeze, .. } | ReorderBody::Group { freeze, .. } => {
                        freeze.as_ref()
                    }
                };
                if freeze.is_some_and(|freeze| {
                    let metadata = worker_state.canonical_metadata.blocking_read();
                    freeze.base.order != metadata.workspace_ordering
                        || freeze.base.repositories != metadata.repository_order
                }) {
                    return Ok(());
                }
                outcome = apply_canonical_reorder(rows, groups, &body, worker_state.cityhall_mode)?;
                // CityHall may regroup a row but not rewrite the shared order, which it
                // denies through `PUT /api/workspace-ordering`. The order is written before
                // the move commits, and put back when the commit fails, so a retry finds
                // the row and its rank where they were.
                if worker_state.cityhall_mode || !matches!(outcome, ReorderOutcome::Moved { .. }) {
                    return Ok(());
                }
                let frozen = freeze.map(|freeze| freeze.order.clone());
                replaced_order = match &body {
                    ReorderBody::Session {
                        id,
                        source_group,
                        destination,
                        direction,
                        ..
                    } => place_carried_workspace(
                        &worker_state,
                        rows,
                        id,
                        destination.as_deref().unwrap_or(source_group),
                        *direction,
                        frozen,
                    )?,
                    ReorderBody::Group { .. } => match frozen {
                        Some(order) => Some(commit_workspace_order(&worker_state, |_| order)?),
                        None => None,
                    },
                };
                Ok(())
            });
            if let (Err(_), Some(previous)) = (&committed, replaced_order) {
                commit_workspace_order(&worker_state, |_| previous)?;
            }
            committed?;
            Ok(outcome)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        }
    };
    drop(namespace);
    match result {
        Ok(outcome) => {
            if let Some((profile, group, direction)) = &reordered_group {
                tracing::info!(target: "session.sync", %profile, %group, %direction, ?outcome, "group reordered");
            }
            match state.runtime.publish(&state).await {
                Ok(snapshot) => {
                    crate::server::runtime::mutation_response(&snapshot.value.cursor, Json(outcome))
                }
                Err(error) => failure(&state, error).await,
            }
        }
        Err(error) => failure(&state, error).await,
    }
}

/// Put a carried session's workspace at the edge of `destination` it entered from, so the
/// shared Custom order ranks it there. Sessions sharing the workspace move with it. A
/// `frozen` order replaces the stored one. Runs inside the carry's session commit, which
/// holds the publication guard; `rows` are the carried profile's rows with the move
/// applied. Returns the stored order it replaced.
fn place_carried_workspace(
    state: &AppState,
    rows: &[Instance],
    id: &str,
    destination: &str,
    direction: crate::daemon::MoveDirection,
    frozen: Option<Vec<String>>,
) -> Result<Option<Vec<String>>> {
    let Some(moved) = rows
        .iter()
        .find(|row| row.id == id)
        .map(|row| row.workspace_id())
    else {
        return Ok(None);
    };
    let peers: Vec<String> = rows
        .iter()
        .filter(|row| row.group_path == destination && !row.is_archived() && !row.is_trashed())
        .map(|row| row.workspace_id())
        .filter(|workspace| *workspace != moved)
        .collect();
    if peers.is_empty() && frozen.is_none() {
        return Ok(None);
    }
    let created: Vec<_> = state
        .instances
        .blocking_read()
        .iter()
        .map(|row| (row.workspace_id(), Some(row.created_at)))
        .collect();
    commit_workspace_order(state, |stored| {
        let order =
            frozen.unwrap_or_else(|| super::sessions::merge_workspace_ordering(created, stored));
        place_at_entering_edge(order.clone(), &moved, &peers, direction).unwrap_or(order)
    })
    .map(Some)
}

/// Replace the stored row order with `next(stored)` and return the order it replaced. A
/// failed write degrades the runtime, since the stored order is then unknown.
fn commit_workspace_order(
    state: &AppState,
    next: impl FnOnce(&[String]) -> Vec<String>,
) -> Result<Vec<String>> {
    let mut previous = Vec::new();
    let committed = crate::session::update_workspace_ordering(|ordering| {
        let replacement = next(&ordering.order);
        previous = std::mem::replace(&mut ordering.order, replacement);
        Ok(())
    });
    let (_, ordering) = match committed {
        Ok(committed) => committed,
        Err(error) => {
            *state.canonical_health.blocking_write() = RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
            state.runtime.request_publish();
            return Err(error);
        }
    };
    state.canonical_metadata.blocking_write().workspace_ordering = ordering.order;
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(previous)
}

/// `order` with `moved` at the edge of the `peers` workspaces a carry enters from: before
/// the first moving down, after the last moving up.
pub(crate) fn place_at_entering_edge(
    mut order: Vec<String>,
    moved: &str,
    peers: &[String],
    direction: crate::daemon::MoveDirection,
) -> Option<Vec<String>> {
    order.retain(|workspace| workspace != moved);
    let mut ranks = order
        .iter()
        .enumerate()
        .filter(|(_, workspace)| peers.contains(workspace))
        .map(|(at, _)| at);
    let at = match direction {
        crate::daemon::MoveDirection::Down => ranks.next()?,
        crate::daemon::MoveDirection::Up => ranks.next_back()? + 1,
    };
    order.insert(at, moved.to_owned());
    Some(order)
}

fn apply_canonical_reorder(
    rows: &mut [Instance],
    groups: &mut Vec<crate::session::Group>,
    body: &crate::daemon::ReorderBody,
    cityhall: bool,
) -> Result<crate::daemon::ReorderOutcome> {
    use crate::daemon::{MoveDirection, ReorderBody, ReorderOutcome};
    match body {
        ReorderBody::Group {
            group,
            direction,
            siblings,
            ..
        } => {
            if cityhall {
                let parent = group.path.rsplit_once('/').map_or("", |(parent, _)| parent);
                for row in rows.iter().filter(|row| {
                    row.group_path != parent
                        && (parent.is_empty() || belongs(&row.group_path, parent))
                }) {
                    check_structured(row)?;
                }
            }
            let mut tree = GroupTree::new_with_groups(rows, groups);
            if !tree.group_exists(&group.path) {
                return Ok(ReorderOutcome::Stale);
            }
            if let Some(siblings) = siblings {
                tree.arrange_siblings(&group.path, siblings);
            }
            let delta = match direction {
                MoveDirection::Up => -1,
                MoveDirection::Down => 1,
            };
            if !tree.move_group(&group.path, delta, siblings.as_deref()) {
                return Ok(ReorderOutcome::AtEdge);
            }
            *groups = tree.get_all_groups();
            Ok(ReorderOutcome::Moved { destination: None })
        }
        ReorderBody::Session {
            id,
            profile,
            source_group,
            destination,
            headers,
            ..
        } => {
            let destination = destination.as_ref().unwrap_or(source_group);
            let Some(anchor) = rows.iter().position(|row| {
                &row.id == id
                    && &row.source_profile == profile
                    && &row.group_path == source_group
                    && !row.is_archived()
                    && !row.is_trashed()
            }) else {
                return Ok(ReorderOutcome::Stale);
            };
            check_row(&rows[anchor], cityhall)?;
            if destination == source_group
                || !(destination.is_empty()
                    || groups.iter().any(|group| &group.path == destination)
                    || rows.iter().any(|row| {
                        &row.source_profile == profile
                            && belongs(&row.group_path, destination)
                            && !row.is_archived()
                            && !row.is_trashed()
                    }))
            {
                return Ok(ReorderOutcome::Stale);
            }
            if let Some(headers) = headers {
                let parent = |path: &str| path.rsplit_once('/').map(|(head, _)| head.to_owned());
                let mut tree = GroupTree::new_with_groups(rows, groups);
                let mut arranged = std::collections::HashSet::new();
                for header in headers {
                    if arranged.insert(parent(header)) {
                        tree.arrange_siblings(header, headers);
                    }
                }
                *groups = tree.get_all_groups();
            }
            rows[anchor].group_path = destination.clone();
            // Reveal only explicit persisted ancestors; implicit paths are synthesized
            // from the newly committed row by the normal GroupTree projection.
            for group in groups.iter_mut() {
                if !group.path.is_empty() && belongs(destination, &group.path) {
                    group.collapsed = false;
                }
            }
            Ok(ReorderOutcome::Moved {
                destination: Some(GroupLocation {
                    profile: profile.clone(),
                    path: destination.clone(),
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::place_at_entering_edge;
    use crate::daemon::MoveDirection::{Down, Up};

    #[test]
    fn carried_workspace_lands_at_the_entering_edge() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (order, peers, direction, expected) in [
            (
                &["m", "a", "p", "b", "q"][..],
                &["p", "q"][..],
                Down,
                Some(&["a", "m", "p", "b", "q"][..]),
            ),
            (
                &["m", "a", "p", "b", "q", "c"],
                &["p", "q"],
                Up,
                Some(&["a", "p", "b", "q", "m", "c"]),
            ),
            (&["p", "m", "q"], &["q", "p"], Down, Some(&["m", "p", "q"])),
            (&["p", "m", "q"], &["p", "q"], Up, Some(&["p", "q", "m"])),
            (&["a", "m"], &["m"], Down, None),
            (&["a", "m"], &[], Up, None),
        ] {
            assert_eq!(
                place_at_entering_edge(list(order), "m", &list(peers), direction),
                expected.map(list),
                "{order:?} into {peers:?} moving {direction:?}"
            );
        }
    }
}

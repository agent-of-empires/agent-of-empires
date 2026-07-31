//! Web CRUD for the project registry. Backs the dashboard's Projects page
//! and feeds the session-creation wizard's multi-select picker.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::session::projects::{self, RegistryError};
use crate::session::{Project, ProjectScope};

use super::AppState;

#[derive(Serialize)]
pub struct ProjectResponse {
    pub name: String,
    pub path: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether the project shows as a sessionless sidebar header. The web
    /// derives the pin marker and empty-header visibility from this. See #2208.
    pub pinned: bool,
}

impl From<Project> for ProjectResponse {
    fn from(p: Project) -> Self {
        Self {
            name: p.name,
            path: p.path,
            scope: p.scope.as_str().to_string(),
            default_base_branch: p.default_base_branch,
            pinned: p.pinned,
        }
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// Optional scope filter: "global", "profile", or omitted (= all).
    #[serde(default)]
    pub scope: Option<String>,
}

#[tracing::instrument(target = "http.api.projects", skip_all, fields(scope = q.scope.as_deref().unwrap_or("merged")))]
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let result: anyhow::Result<Vec<Project>> = match q.scope.as_deref() {
        Some("global") => projects::load_global(),
        Some("profile") => projects::load_profile(&state.profile),
        Some(other) => {
            tracing::warn!(target: "http.api.projects", scope = other, "rejected bad scope");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_scope",
                    "message": format!("Unknown scope '{}'. Use 'global', 'profile', or omit.", other),
                })),
            )
                .into_response();
        }
        None => projects::load_merged(&state.profile),
    };

    match result {
        Ok(list) => {
            tracing::debug!(target: "http.api.projects", count = list.len(), "listed projects");
            Json(
                list.into_iter()
                    .map(ProjectResponse::from)
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.projects", error = %e, "load_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "load_failed", "message": e.to_string()})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CreateProjectBody {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    /// "global" (default) or "profile".
    #[serde(default)]
    pub scope: Option<String>,
    /// When true, allow registering this path even if it already exists in
    /// the other scope. Defaults to false; cross-scope path collisions
    /// otherwise return 409.
    #[serde(default)]
    pub allow_override: bool,
    /// Default base branch for new worktree branches created against this
    /// project, whether it is the launch repo or an extra repo in a multi-repo
    /// workspace. Empty/whitespace is treated as unset.
    #[serde(default)]
    pub default_base_branch: Option<String>,
    /// Whether to pin the project (show it as a sessionless sidebar header).
    /// Defaults to false: the Projects view just saves a project, while the
    /// sidebar "Pin project" action sends `true`. See #2208.
    #[serde(default)]
    pub pinned: bool,
}

#[tracing::instrument(
    target = "http.api.projects",
    skip_all,
    fields(
        path = tracing::field::Empty,
        scope = tracing::field::Empty,
        allow_override = tracing::field::Empty,
    ),
)]
pub async fn create_project(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CreateProjectBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        tracing::warn!(target: "http.api.projects", reason = "cityhall_mode", "rejected create");
        return resp;
    }
    if state.read_only {
        tracing::warn!(target: "http.api.projects", reason = "read_only", "rejected create");
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let span = tracing::Span::current();
    span.record("path", body.path.as_str());
    span.record("scope", body.scope.as_deref().unwrap_or("global"));
    span.record("allow_override", body.allow_override);

    let scope = match body.scope.as_deref() {
        Some("profile") => ProjectScope::Profile,
        Some("global") | None => ProjectScope::Global,
        Some(other) => {
            tracing::warn!(target: "http.api.projects", scope = other, "rejected bad scope");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_scope",
                    "message": format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
                })),
            )
                .into_response();
        }
    };

    let path_buf = std::path::PathBuf::from(&body.path);
    let canonical = path_buf.canonicalize().unwrap_or_else(|_| path_buf.clone());

    let name = body.name.unwrap_or_else(|| {
        canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string())
    });

    // Non-git directories are allowed: their sessions run in place, with no
    // worktrees or branches. We still reject paths that don't resolve to a
    // directory, which the previous git-repo gate rejected implicitly.
    if !canonical.is_dir() {
        tracing::warn!(target: "http.api.projects", path = %canonical.display(), "rejected non-directory path");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "not_a_directory",
                "message": format!("Path does not exist or is not a directory: {}", canonical.display()),
            })),
        )
            .into_response();
    }

    let project = Project::new(name, canonical.to_string_lossy(), scope)
        .with_base_branch(body.default_base_branch)
        .with_pinned(body.pinned);
    match projects::add(&state.profile, scope, project, body.allow_override) {
        Ok(saved) => {
            tracing::info!(target: "http.api.projects", name = %saved.name, path = %saved.path, scope = saved.scope.as_str(), "created project");
            (StatusCode::CREATED, Json(ProjectResponse::from(saved))).into_response()
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected create");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "conflict", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected create");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "add_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "add_failed", "message": e.to_string()})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    /// "global" (default) or "profile".
    #[serde(default)]
    pub scope: Option<String>,
}

#[tracing::instrument(target = "http.api.projects", skip_all, fields(name = %name, scope = q.scope.as_deref().unwrap_or("global")))]
pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        tracing::warn!(target: "http.api.projects", reason = "cityhall_mode", "rejected delete");
        return resp;
    }
    if state.read_only {
        tracing::warn!(target: "http.api.projects", reason = "read_only", "rejected delete");
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }

    let scope = match q.scope.as_deref() {
        Some("profile") => ProjectScope::Profile,
        Some("global") | None => ProjectScope::Global,
        Some(other) => {
            tracing::warn!(target: "http.api.projects", scope = other, "rejected bad scope");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_scope",
                    "message": format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
                })),
            )
                .into_response();
        }
    };

    match projects::remove(&state.profile, scope, &name) {
        Ok(removed) => {
            tracing::info!(target: "http.api.projects", name = %removed.name, path = %removed.path, scope = removed.scope.as_str(), "deleted project");
            (StatusCode::OK, Json(ProjectResponse::from(removed))).into_response()
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected delete");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected delete");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "conflict", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "remove_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "remove_failed", "message": e.to_string()})),
            )
                .into_response()
        }
    }
}

/// Parsed PATCH body for a project. Each field is `None` when its key is
/// absent (leave that attribute untouched), `Some(_)` when present. The raw
/// JSON is inspected rather than deserialized into a struct because a missing
/// `default_base_branch` key must be distinguishable from an explicit `null`
/// (which clears the value); serde would fold both to `None`. At least one
/// recognized key must be present, else the request is a no-op.
#[derive(Debug, PartialEq)]
struct ProjectPatch {
    /// `None`: key absent. `Some(None)`: clear. `Some(Some(s))`: set to `s`
    /// (empty/whitespace normalized to unset downstream).
    base_branch: Option<Option<String>>,
    /// `None`: key absent. `Some(b)`: set the pin flag to `b`.
    pinned: Option<bool>,
}

fn parse_project_patch(
    body: &serde_json::Value,
) -> Result<ProjectPatch, (&'static str, &'static str)> {
    let base_branch = match body.get("default_base_branch") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(s)) => Some(Some(s.clone())),
        Some(_) => return Err(("bad_field", "default_base_branch must be a string or null")),
    };
    let pinned = match body.get("pinned") {
        None => None,
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(_) => return Err(("bad_field", "pinned must be a boolean")),
    };
    if base_branch.is_none() && pinned.is_none() {
        return Err((
            "no_fields",
            "provide at least one of: default_base_branch, pinned",
        ));
    }
    Ok(ProjectPatch {
        base_branch,
        pinned,
    })
}

#[tracing::instrument(target = "http.api.projects", skip_all, fields(name = %name, scope = q.scope.as_deref().unwrap_or("global")))]
pub async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        tracing::warn!(target: "http.api.projects", reason = "cityhall_mode", "rejected update");
        return resp;
    }
    if state.read_only {
        tracing::warn!(target: "http.api.projects", reason = "read_only", "rejected update");
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }

    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let scope = match q.scope.as_deref() {
        Some("profile") => ProjectScope::Profile,
        Some("global") | None => ProjectScope::Global,
        Some(other) => {
            tracing::warn!(target: "http.api.projects", scope = other, "rejected bad scope");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_scope",
                    "message": format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
                })),
            )
                .into_response();
        }
    };

    let patch = match parse_project_patch(&body) {
        Ok(patch) => patch,
        Err((err, msg)) => {
            tracing::warn!(target: "http.api.projects", reason = err, "rejected update");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": err, "message": msg })),
            )
                .into_response();
        }
    };

    // Apply each present field in turn. Both are read-modify-write over the
    // same registry file, so the last call's returned project reflects every
    // applied change. `parse_project_patch` guarantees at least one field.
    let mut result: Option<std::result::Result<Project, RegistryError>> = None;
    if let Some(base) = patch.base_branch {
        result = Some(projects::update_base_branch(
            &state.profile,
            scope,
            &name,
            base,
        ));
    }
    if let Some(pinned) = patch.pinned {
        // Don't run the pinned write if a prior base-branch write already
        // failed (e.g. NotFound), so its error is surfaced rather than masked.
        if !matches!(&result, Some(Err(_))) {
            result = Some(projects::set_pinned(&state.profile, scope, &name, pinned));
        }
    }

    match result.expect("parse_project_patch guarantees at least one field") {
        Ok(updated) => {
            tracing::info!(target: "http.api.projects", name = %updated.name, path = %updated.path, scope = updated.scope.as_str(), "updated project");
            (StatusCode::OK, Json(ProjectResponse::from(updated))).into_response()
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected update");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected update");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "conflict", "message": msg})),
            )
                .into_response()
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "update_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "update_failed", "message": e.to_string()})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_project_patch, ProjectPatch};
    use serde_json::json;

    #[test]
    fn project_patch_requires_at_least_one_field() {
        // An empty body is a no-op, not an intent to clear: this guard stops a
        // `{}` body from silently wiping the default base branch (#2208 made
        // the base-branch key optional, so the old "missing_field" guard moved
        // here as "no_fields").
        assert_eq!(
            parse_project_patch(&json!({})),
            Err((
                "no_fields",
                "provide at least one of: default_base_branch, pinned"
            ))
        );
    }

    #[test]
    fn project_patch_parses_base_branch() {
        // null clears, a string sets; the key being present is what matters.
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": null})),
            Ok(ProjectPatch {
                base_branch: Some(None),
                pinned: None
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": "develop"})),
            Ok(ProjectPatch {
                base_branch: Some(Some("develop".to_string())),
                pinned: None
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": 42})),
            Err(("bad_field", "default_base_branch must be a string or null"))
        );
    }

    #[test]
    fn project_patch_parses_pinned_alone() {
        // The unpin path sends only `pinned`, with no base-branch key.
        assert_eq!(
            parse_project_patch(&json!({"pinned": false})),
            Ok(ProjectPatch {
                base_branch: None,
                pinned: Some(false)
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"pinned": "yes"})),
            Err(("bad_field", "pinned must be a boolean"))
        );
    }
}

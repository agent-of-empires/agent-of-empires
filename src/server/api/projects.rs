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

use crate::git::GitWorktree;
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
}

impl From<Project> for ProjectResponse {
    fn from(p: Project) -> Self {
        Self {
            name: p.name,
            path: p.path,
            scope: p.scope.as_str().to_string(),
            default_base_branch: p.default_base_branch,
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
    /// project in a multi-repo workspace. Empty/whitespace is treated as unset.
    #[serde(default)]
    pub default_base_branch: Option<String>,
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
    if !GitWorktree::is_git_repo(&canonical) {
        tracing::warn!(target: "http.api.projects", path = %canonical.display(), "rejected non-git-repo");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "not_a_git_repo",
                "message": format!("Path is not a git repository: {}", canonical.display()),
            })),
        )
            .into_response();
    }

    let name = body.name.unwrap_or_else(|| {
        canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string())
    });

    let project = Project::new(name, canonical.to_string_lossy(), scope)
        .with_base_branch(body.default_base_branch);
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

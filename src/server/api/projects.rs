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
use crate::session::{Project, ProjectOverrides, ProjectScope};

use super::AppState;
use super::{api_error, read_only_response};

#[derive(Serialize)]
pub struct ProjectResponse {
    pub name: String,
    pub path: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether the project shows as a sessionless sidebar header, which drives
    /// the pin marker and empty-header visibility (#2208).
    pub pinned: bool,
    #[serde(skip_serializing_if = "ProjectOverrides::is_empty")]
    pub overrides: ProjectOverrides,
}

impl From<Project> for ProjectResponse {
    fn from(p: Project) -> Self {
        Self {
            name: p.name,
            path: p.path,
            scope: p.scope.as_str().to_string(),
            default_base_branch: p.default_base_branch,
            pinned: p.pinned,
            overrides: p.overrides,
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
            return api_error(
                StatusCode::BAD_REQUEST,
                "bad_scope",
                format!(
                    "Unknown scope '{}'. Use 'global', 'profile', or omit.",
                    other
                ),
            );
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
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                e.to_string(),
            )
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
    /// Allow registering this path even when it already exists in the other
    /// scope. Cross-scope path collisions otherwise return 409.
    #[serde(default)]
    pub allow_override: bool,
    /// Default base branch for new worktree branches created against this
    /// project. Empty or whitespace is treated as unset.
    #[serde(default)]
    pub default_base_branch: Option<String>,
    /// Pin the project as a sessionless sidebar header. The Projects view just
    /// saves a project; the sidebar "Pin project" action sends `true` (#2208).
    #[serde(default)]
    pub pinned: bool,
    /// Per-project overrides for otherwise-global settings. Absent/empty
    /// means "don't override anything".
    #[serde(default)]
    pub overrides: ProjectOverrides,
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
        return read_only_response();
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
            return api_error(
                StatusCode::BAD_REQUEST,
                "bad_scope",
                format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
            );
        }
    };

    let path_buf = std::path::PathBuf::from(&body.path);
    let canonical = path_buf.canonicalize().unwrap_or_else(|_| path_buf.clone());

    let name = match body.name {
        Some(n) => n,
        None => {
            let base = canonical
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            projects::unique_name(&state.profile, scope, &base)
        }
    };

    // Non-git directories are allowed: their sessions run in place. A path that
    // does not resolve to a directory is still rejected.
    if !canonical.is_dir() {
        tracing::warn!(target: "http.api.projects", path = %canonical.display(), "rejected non-directory path");
        return api_error(
            StatusCode::BAD_REQUEST,
            "not_a_directory",
            format!(
                "Path does not exist or is not a directory: {}",
                canonical.display()
            ),
        );
    }

    let project = Project::new(name, canonical.to_string_lossy(), scope)
        .with_base_branch(body.default_base_branch)
        .with_pinned(body.pinned)
        .with_overrides(body.overrides);
    match projects::add(&state.profile, scope, project, body.allow_override) {
        Ok(saved) => {
            tracing::info!(target: "http.api.projects", name = %saved.name, path = %saved.path, scope = saved.scope.as_str(), "created project");
            (StatusCode::CREATED, Json(ProjectResponse::from(saved))).into_response()
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected create");
            api_error(StatusCode::CONFLICT, "conflict", msg)
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected create");
            api_error(StatusCode::NOT_FOUND, "not_found", msg)
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "add_failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "add_failed",
                e.to_string(),
            )
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
        return read_only_response();
    }

    let scope = match q.scope.as_deref() {
        Some("profile") => ProjectScope::Profile,
        Some("global") | None => ProjectScope::Global,
        Some(other) => {
            tracing::warn!(target: "http.api.projects", scope = other, "rejected bad scope");
            return api_error(
                StatusCode::BAD_REQUEST,
                "bad_scope",
                format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
            );
        }
    };

    match projects::remove(&state.profile, scope, &name) {
        Ok(removed) => {
            tracing::info!(target: "http.api.projects", name = %removed.name, path = %removed.path, scope = removed.scope.as_str(), "deleted project");
            (StatusCode::OK, Json(ProjectResponse::from(removed))).into_response()
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected delete");
            api_error(StatusCode::NOT_FOUND, "not_found", msg)
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected delete");
            api_error(StatusCode::CONFLICT, "conflict", msg)
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "remove_failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "remove_failed",
                e.to_string(),
            )
        }
    }
}

/// Parsed PATCH body for a project. The raw JSON is inspected rather than
/// deserialized because a missing `default_base_branch` key must be
/// distinguishable from an explicit `null` (which clears the value), and serde
/// folds both to `None`. At least one recognized key must be present.
#[derive(Debug, PartialEq)]
struct ProjectPatch {
    /// `None`: key absent. `Some(None)`: clear. `Some(Some(s))`: set to `s`
    /// (empty/whitespace normalized to unset downstream).
    base_branch: Option<Option<String>>,
    /// `None`: key absent. `Some(b)`: set the pin flag to `b`.
    pinned: Option<bool>,
    /// `None`: `overrides` key absent (leave untouched). `Some(patch)`: apply
    /// each present sub-field of `patch` (same absent/null/value semantics as
    /// the top-level fields, scoped per override).
    overrides: Option<OverridesPatch>,
}

/// Per-sub-field absent/null/value patch for `overrides`, mirroring
/// [`ProjectPatch`]'s semantics one level deeper.
#[derive(Debug, PartialEq)]
struct OverridesPatch {
    worktree_enabled: Option<Option<bool>>,
    smart_rename: Option<Option<bool>>,
}

impl OverridesPatch {
    fn is_empty(&self) -> bool {
        self.worktree_enabled.is_none() && self.smart_rename.is_none()
    }
}

fn parse_overrides_patch(
    body: &serde_json::Value,
) -> Result<OverridesPatch, (&'static str, &'static str)> {
    let worktree_enabled = match body.get("worktree_enabled") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::Bool(b)) => Some(Some(*b)),
        Some(_) => {
            return Err((
                "bad_field",
                "overrides.worktree_enabled must be a boolean or null",
            ))
        }
    };
    let smart_rename = match body.get("smart_rename") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::Bool(b)) => Some(Some(*b)),
        Some(_) => {
            return Err((
                "bad_field",
                "overrides.smart_rename must be a boolean or null",
            ))
        }
    };
    Ok(OverridesPatch {
        worktree_enabled,
        smart_rename,
    })
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
    let overrides = match body.get("overrides") {
        None => None,
        Some(v @ serde_json::Value::Object(_)) => {
            let patch = parse_overrides_patch(v)?;
            if patch.is_empty() {
                None
            } else {
                Some(patch)
            }
        }
        Some(_) => return Err(("bad_field", "overrides must be an object")),
    };
    if base_branch.is_none() && pinned.is_none() && overrides.is_none() {
        return Err((
            "no_fields",
            "provide at least one of: default_base_branch, pinned, overrides",
        ));
    }
    Ok(ProjectPatch {
        base_branch,
        pinned,
        overrides,
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
        return read_only_response();
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
            return api_error(
                StatusCode::BAD_REQUEST,
                "bad_scope",
                format!("Unknown scope '{}'. Use 'global' or 'profile'.", other),
            );
        }
    };

    let patch = match parse_project_patch(&body) {
        Ok(patch) => patch,
        Err((err, msg)) => {
            tracing::warn!(target: "http.api.projects", reason = err, "rejected update");
            return api_error(StatusCode::BAD_REQUEST, err, msg);
        }
    };

    // Apply each present field in turn. Both are read-modify-write over the
    // same registry file, so the last call's returned project reflects every
    // change. `parse_project_patch` guarantees at least one field.
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
        // Skip the pinned write if a prior base-branch write failed, so its
        // error is surfaced rather than masked.
        if !matches!(&result, Some(Err(_))) {
            result = Some(projects::set_pinned(&state.profile, scope, &name, pinned));
        }
    }
    if let Some(overrides) = patch.overrides {
        if !matches!(&result, Some(Err(_))) {
            result = Some(projects::update_overrides(
                &state.profile,
                scope,
                &name,
                |ov| {
                    if let Some(w) = overrides.worktree_enabled {
                        ov.worktree_enabled = w;
                    }
                    if let Some(s) = overrides.smart_rename {
                        ov.smart_rename = s;
                    }
                },
            ));
        }
    }

    match result.expect("parse_project_patch guarantees at least one field") {
        Ok(updated) => {
            tracing::info!(target: "http.api.projects", name = %updated.name, path = %updated.path, scope = updated.scope.as_str(), "updated project");
            (StatusCode::OK, Json(ProjectResponse::from(updated))).into_response()
        }
        Err(RegistryError::NotFound(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "not_found", message = %msg, "rejected update");
            api_error(StatusCode::NOT_FOUND, "not_found", msg)
        }
        Err(RegistryError::Conflict(msg)) => {
            tracing::warn!(target: "http.api.projects", reason = "conflict", message = %msg, "rejected update");
            api_error(StatusCode::CONFLICT, "conflict", msg)
        }
        Err(RegistryError::Other(e)) => {
            tracing::error!(target: "http.api.projects", error = %e, "update_failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "update_failed",
                e.to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_project_patch, OverridesPatch, ProjectPatch};
    use serde_json::json;

    #[test]
    fn project_patch_requires_at_least_one_field() {
        // An empty body is a no-op, not an intent to clear: this stops a `{}`
        // body from silently wiping the default base branch (#2208).
        assert_eq!(
            parse_project_patch(&json!({})),
            Err((
                "no_fields",
                "provide at least one of: default_base_branch, pinned, overrides"
            ))
        );
    }

    #[test]
    fn project_patch_parses_each_field() {
        // null clears, a string sets; the key being present is what matters.
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": null})),
            Ok(ProjectPatch {
                base_branch: Some(None),
                pinned: None,
                overrides: None,
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": "develop"})),
            Ok(ProjectPatch {
                base_branch: Some(Some("develop".to_string())),
                pinned: None,
                overrides: None,
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"default_base_branch": 42})),
            Err(("bad_field", "default_base_branch must be a string or null"))
        );
        // The unpin path sends only `pinned`, with no base-branch key.
        assert_eq!(
            parse_project_patch(&json!({"pinned": false})),
            Ok(ProjectPatch {
                base_branch: None,
                pinned: Some(false),
                overrides: None,
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"pinned": "yes"})),
            Err(("bad_field", "pinned must be a boolean"))
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {"worktree_enabled": true}})),
            Ok(ProjectPatch {
                base_branch: None,
                pinned: None,
                overrides: Some(OverridesPatch {
                    worktree_enabled: Some(Some(true)),
                    smart_rename: None,
                }),
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {"smart_rename": null}})),
            Ok(ProjectPatch {
                base_branch: None,
                pinned: None,
                overrides: Some(OverridesPatch {
                    worktree_enabled: None,
                    smart_rename: Some(None),
                }),
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {}})),
            Err((
                "no_fields",
                "provide at least one of: default_base_branch, pinned, overrides"
            ))
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {"worktree_enabled": "yes"}})),
            Err((
                "bad_field",
                "overrides.worktree_enabled must be a boolean or null"
            ))
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": "nope"})),
            Err(("bad_field", "overrides must be an object"))
        );
    }
}

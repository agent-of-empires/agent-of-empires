//! Web CRUD for the project registry. Backs the dashboard's Projects page
//! and feeds the session-creation wizard's multi-select picker.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::server::auth::{handler_elevated, AuthenticatedBrowserSession, LoopbackTrusted};
use crate::session::config::settings_schema::validate_project_plugin_setting;
use crate::session::projects::{self, RegistryError};
use crate::session::{Project, ProjectOverrides, ProjectScope};

use super::AppState;
use super::{api_error, read_only_response, validate_profile_name};

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
    /// Profile whose registry entries to resolve; defaults to the served profile.
    #[serde(default)]
    pub profile: Option<String>,
}

#[tracing::instrument(target = "http.api.projects", skip_all, fields(scope = q.scope.as_deref().unwrap_or("merged")))]
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let profile = match q.profile.as_deref() {
        Some(name) => {
            if let Err(e) = validate_profile_name(name) {
                return api_error(StatusCode::BAD_REQUEST, "bad_profile", e);
            }
            name
        }
        None => state.profile.as_str(),
    };
    let result: anyhow::Result<Vec<Project>> = match q.scope.as_deref() {
        Some("global") => projects::load_global(),
        Some("profile") => projects::load_profile(profile),
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
        // The wizard derives sandbox and worktree defaults from this, so an unreadable registry must
        // fail the request instead of reading as "no overrides".
        None => projects::load_merged_strict(profile),
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
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
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // A null clears a value, as in PATCH, so it is dropped rather than stored as an override.
    body.overrides.plugin_settings.retain(|_, settings| {
        settings.retain(|_, value| !value.is_null());
        !settings.is_empty()
    });
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

    let plugin_settings: PluginSettingsPatch = body
        .overrides
        .plugin_settings
        .iter()
        .map(|(plugin_id, settings)| {
            let settings = settings
                .iter()
                .map(|(key, value)| (key.clone(), Some(value.clone())))
                .collect();
            (plugin_id.clone(), settings)
        })
        .collect();
    let elevated = handler_elevated(&state, session.as_deref(), loopback.is_some()).await;
    if !elevated && !plugin_settings.values().all(BTreeMap::is_empty) {
        return elevation_required();
    }
    if let Err(msg) = validate_plugin_settings(&plugin_settings) {
        tracing::warn!(target: "http.api.projects", reason = "bad_plugin_setting", message = %msg, "rejected create");
        return api_error(StatusCode::BAD_REQUEST, "bad_plugin_setting", msg);
    }

    let project = Project::new(name, canonical.to_string_lossy(), scope)
        .with_base_branch(body.default_base_branch)
        .with_pinned(body.pinned)
        .with_overrides(body.overrides);
    // A new entry can hide the values of one it shadows. That is decided under the registry locks,
    // together with the write, so no concurrent writer can slip in between.
    let added = projects::add_guarded(
        &state.profile,
        scope,
        project,
        body.allow_override,
        |effect| {
            if elevated || effect.is_noop() {
                Ok(())
            } else {
                Err(RegistryError::Forbidden(PLUGIN_SETTINGS_ELEVATION.into()))
            }
        },
    );
    match added {
        Ok((saved, effect)) => {
            tracing::info!(target: "http.api.projects", name = %saved.name, path = %saved.path, scope = saved.scope.as_str(), "created project");
            emit_effective_changes(&state, &effect).await;
            (StatusCode::CREATED, Json(ProjectResponse::from(saved))).into_response()
        }
        Err(RegistryError::Forbidden(_)) => elevation_required(),
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
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

    // Removing an entry drops its own values and can reveal those of an entry it shadows. That is
    // decided under the registry locks, together with the write, so no concurrent writer can slip
    // in between.
    let elevated = handler_elevated(&state, session.as_deref(), loopback.is_some()).await;
    let removed = projects::remove_guarded(&state.profile, scope, &name, |entry, effect| {
        if elevated || (entry.overrides.plugin_settings.is_empty() && effect.is_noop()) {
            Ok(())
        } else {
            Err(RegistryError::Forbidden(PLUGIN_SETTINGS_ELEVATION.into()))
        }
    });
    match removed {
        Ok((removed, effect)) => {
            tracing::info!(target: "http.api.projects", name = %removed.name, path = %removed.path, scope = removed.scope.as_str(), "deleted project");
            emit_effective_changes(&state, &effect).await;
            (StatusCode::OK, Json(ProjectResponse::from(removed))).into_response()
        }
        Err(RegistryError::Forbidden(_)) => elevation_required(),
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
    sandbox_enabled: Option<Option<bool>>,
    /// Per plugin id, per setting key: `Some(value)` sets, `None` (JSON `null`) clears.
    plugin_settings: PluginSettingsPatch,
}

type PluginSettingsPatch = BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>;

impl OverridesPatch {
    fn is_empty(&self) -> bool {
        self.worktree_enabled.is_none()
            && self.smart_rename.is_none()
            && self.sandbox_enabled.is_none()
            && self.plugin_settings.is_empty()
    }
}

fn parse_plugin_settings_patch(
    body: &serde_json::Value,
) -> Result<PluginSettingsPatch, (&'static str, &'static str)> {
    const BAD: (&str, &str) = (
        "bad_field",
        "overrides.plugin_settings must map plugin ids to objects of setting values",
    );
    let Some(plugins) = body.get("plugin_settings") else {
        return Ok(PluginSettingsPatch::new());
    };
    let plugins = plugins.as_object().ok_or(BAD)?;
    plugins
        .iter()
        .map(|(plugin_id, settings)| {
            let settings = settings.as_object().ok_or(BAD)?;
            let settings = settings
                .iter()
                .map(|(key, value)| (key.clone(), (!value.is_null()).then(|| value.clone())))
                .collect();
            Ok((plugin_id.clone(), settings))
        })
        .collect()
}

const PLUGIN_SETTINGS_ELEVATION: &str = "per-project plugin settings need a recent passphrase";

/// Per-project plugin values steer plugin behaviour like the machine-wide ones, so any set or
/// clear needs the same elevation (`PATCH /api/settings` gates those). So does anything that would
/// drop, reveal or shadow stored values: removing an entry, or one overriding another at its path.
fn elevation_required() -> axum::response::Response {
    tracing::warn!(target: "http.api.projects", reason = "elevation_required", "rejected plugin settings write");
    api_error(
        StatusCode::FORBIDDEN,
        "elevation_required",
        PLUGIN_SETTINGS_ELEVATION,
    )
}

/// Reject any value for a setting its plugin does not declare `per_project`, or that
/// fails the same validation a global write gets. Clears (`None`) are always allowed.
fn validate_plugin_settings(patch: &PluginSettingsPatch) -> Result<(), String> {
    let registry = crate::plugin::registry();
    for (plugin_id, settings) in patch {
        let plugin = registry.get(plugin_id).filter(|p| p.active());
        for (key, value) in settings {
            let Some(value) = value else { continue };
            let decl = plugin.and_then(|p| p.manifest.settings.iter().find(|s| &s.key == key));
            validate_project_plugin_setting(plugin_id, key, decl, value)?;
        }
    }
    Ok(())
}

/// Tell each plugin's worker which per-project keys changed in effect at the path.
async fn emit_effective_changes(state: &AppState, change: &projects::EffectiveChange) {
    let Some(host) = &state.plugin_host else {
        return;
    };
    host.emit_settings_changed(&change.changed_keys(), Some(&change.path))
        .await;
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
    let sandbox_enabled = match body.get("sandbox_enabled") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::Bool(b)) => Some(Some(*b)),
        Some(_) => {
            return Err((
                "bad_field",
                "overrides.sandbox_enabled must be a boolean or null",
            ))
        }
    };
    Ok(OverridesPatch {
        worktree_enabled,
        smart_rename,
        sandbox_enabled,
        plugin_settings: parse_plugin_settings_patch(body)?,
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
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

    let plugin_settings = patch
        .overrides
        .as_ref()
        .map(|o| o.plugin_settings.clone())
        .unwrap_or_default();
    let writes_plugin_settings = !plugin_settings.values().all(BTreeMap::is_empty);
    if writes_plugin_settings
        && !handler_elevated(&state, session.as_deref(), loopback.is_some()).await
    {
        return elevation_required();
    }
    if let Err(msg) = validate_plugin_settings(&plugin_settings) {
        tracing::warn!(target: "http.api.projects", reason = "bad_plugin_setting", message = %msg, "rejected update");
        return api_error(StatusCode::BAD_REQUEST, "bad_plugin_setting", msg);
    }

    // The change in effect at the path, committed with the write (a shadowed entry's edit
    // changes nothing in effect).
    let mut effect: Option<projects::EffectiveChange> = None;

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
            let apply = |ov: &mut ProjectOverrides| {
                if let Some(w) = overrides.worktree_enabled {
                    ov.worktree_enabled = w;
                }
                if let Some(s) = overrides.smart_rename {
                    ov.smart_rename = s;
                }
                if let Some(c) = overrides.sandbox_enabled {
                    ov.sandbox_enabled = c;
                }
                for (plugin_id, settings) in overrides.plugin_settings {
                    for (key, value) in settings {
                        ov.set_plugin_setting(&plugin_id, &key, value);
                    }
                }
            };
            result = Some(if plugin_settings.is_empty() {
                projects::update_overrides(&state.profile, scope, &name, apply)
            } else {
                projects::update_overrides_with_effect(&state.profile, scope, &name, apply).map(
                    |(project, change)| {
                        effect = Some(change);
                        project
                    },
                )
            });
        }
    }

    match result.expect("parse_project_patch guarantees at least one field") {
        Ok(updated) => {
            tracing::info!(target: "http.api.projects", name = %updated.name, path = %updated.path, scope = updated.scope.as_str(), "updated project");
            if let Some(change) = &effect {
                emit_effective_changes(&state, change).await;
            }
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
        Err(RegistryError::Forbidden(_)) => elevation_required(),
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

    #[tokio::test]
    #[serial_test::serial]
    async fn plugin_setting_writes_need_elevation_but_other_overrides_do_not() {
        use super::*;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        projects::add(
            "test",
            ProjectScope::Global,
            Project::new("demo", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )
        .unwrap();
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).expect("fresh state").login_manager =
            Arc::new(crate::server::login::LoginManager::new(Some("pw")));
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let patch = |body: serde_json::Value| {
            let state = state.clone();
            let session = session.clone();
            async move {
                let resp = update_project(
                    State(state),
                    Path("demo".to_string()),
                    Query(DeleteQuery { scope: None }),
                    Some(axum::Extension(AuthenticatedBrowserSession(session))),
                    None,
                    Ok(Json(body)),
                )
                .await
                .into_response();
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let error = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_owned));
                (status, error)
            }
        };
        let plugin = json!({"overrides": {"plugin_settings": {"acme.kit": {"depth": 1}}}});
        let clear = json!({"overrides": {"plugin_settings": {"acme.kit": {"depth": null}}}});

        for body in [plugin.clone(), clear] {
            let (status, error) = patch(body).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(error.as_deref(), Some("elevation_required"));
        }
        let (status, _) = patch(json!({"overrides": {"smart_rename": true}})).await;
        assert_eq!(status, StatusCode::OK, "other overrides stay non-elevated");

        state.login_manager.elevate_session(&session).await;
        let (status, error) = patch(plugin).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "past the gate, the unknown plugin fails validation"
        );
        assert_eq!(error.as_deref(), Some("bad_plugin_setting"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dropping_or_shadowing_stored_plugin_settings_needs_elevation() {
        use super::*;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let repo = home.path().join("repo");
        let plain = home.path().join("plain");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        for (name, path) in [("demo", &repo), ("plain", &plain)] {
            projects::add(
                "test",
                ProjectScope::Global,
                Project::new(name, path.to_string_lossy(), ProjectScope::Global),
                false,
            )
            .unwrap();
        }
        projects::update_overrides("test", ProjectScope::Global, "demo", |ov| {
            ov.set_plugin_setting("acme.kit", "depth", Some(json!(1)))
        })
        .unwrap();
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).expect("fresh state").login_manager =
            Arc::new(crate::server::login::LoginManager::new(Some("pw")));
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let ext = || {
            Some(axum::Extension(AuthenticatedBrowserSession(
                session.clone(),
            )))
        };
        let delete = |name: &str| {
            delete_project(
                State(state.clone()),
                Path(name.to_string()),
                Query(DeleteQuery { scope: None }),
                ext(),
                None,
            )
        };
        let shadow = |path: &std::path::Path| {
            create_project(
                State(state.clone()),
                ext(),
                None,
                Ok(Json(CreateProjectBody {
                    path: path.to_string_lossy().to_string(),
                    name: Some("shadow".into()),
                    scope: Some("profile".into()),
                    allow_override: true,
                    default_base_branch: None,
                    pinned: false,
                    overrides: ProjectOverrides::default(),
                })),
            )
        };

        let status = |resp: axum::response::Response| resp.status();
        assert_eq!(
            status(delete("demo").await.into_response()),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(shadow(&repo).await.into_response()),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(delete("plain").await.into_response()),
            StatusCode::OK,
            "an entry without plugin settings stays non-elevated"
        );
        assert_eq!(
            status(shadow(&plain).await.into_response()),
            StatusCode::CREATED
        );

        state.login_manager.elevate_session(&session).await;
        assert_eq!(status(delete("demo").await.into_response()), StatusCode::OK);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn removing_an_empty_shadow_needs_elevation_when_it_reveals_values() {
        use super::*;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        projects::add(
            "test",
            ProjectScope::Global,
            Project::new("base", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )
        .unwrap();
        projects::update_overrides("test", ProjectScope::Global, "base", |ov| {
            ov.set_plugin_setting("acme.kit", "depth", Some(json!(7)))
        })
        .unwrap();
        projects::add(
            "test",
            ProjectScope::Profile,
            Project::new("shadow", repo.to_string_lossy(), ProjectScope::Profile),
            true,
        )
        .unwrap();
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).expect("fresh state").login_manager =
            Arc::new(crate::server::login::LoginManager::new(Some("pw")));
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let delete_shadow = || {
            delete_project(
                State(state.clone()),
                Path("shadow".to_string()),
                Query(DeleteQuery {
                    scope: Some("profile".into()),
                }),
                Some(axum::Extension(AuthenticatedBrowserSession(
                    session.clone(),
                ))),
                None,
            )
        };

        assert_eq!(
            delete_shadow().await.into_response().status(),
            StatusCode::FORBIDDEN,
            "the global entry's values would become effective again"
        );
        state.login_manager.elevate_session(&session).await;
        assert_eq!(
            delete_shadow().await.into_response().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn create_drops_null_plugin_values_so_the_project_inherits() {
        use super::*;
        use crate::session::config::settings_schema::resolve_plugin_setting;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).expect("fresh state").login_manager =
            Arc::new(crate::server::login::LoginManager::new(Some("pw")));
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let body: CreateProjectBody = serde_json::from_value(json!({
            "path": repo.to_string_lossy(),
            "overrides": { "plugin_settings": { "acme.kit": { "verbose": null } } },
        }))
        .unwrap();

        let resp = create_project(
            State(state),
            Some(axum::Extension(AuthenticatedBrowserSession(session))),
            None,
            Ok(Json(body)),
        )
        .await
        .into_response();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "a null is no write, so no elevation"
        );

        let stored = projects::find_by_canonical_path("test", &repo).expect("registered");
        assert!(stored.overrides.plugin_settings.is_empty());
        let decl = aoe_plugin_api::SettingContribution {
            per_project: true,
            ..serde_json::from_value(json!({"key": "verbose", "type": "bool"})).unwrap()
        };
        let resolved = resolve_plugin_setting(
            "acme.kit",
            "verbose",
            Some(&decl),
            Some(&stored),
            Some(json!(true)),
        );
        assert_eq!(
            resolved.map(|(value, source)| (value, source.as_str())),
            Some((json!(true), "global"))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn create_and_delete_refuse_to_guess_when_a_registry_is_unreadable() {
        use super::*;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let repo = home.path().join("repo");
        let other = home.path().join("other");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        projects::add(
            "test",
            ProjectScope::Profile,
            Project::new("p", repo.to_string_lossy(), ProjectScope::Profile),
            false,
        )
        .unwrap();
        // The global registry is the unreadable one; the writes below only touch the profile one.
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("projects.json"),
            "{ not json",
        )
        .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());

        let resp = delete_project(
            State(state.clone()),
            Path("p".to_string()),
            Query(DeleteQuery {
                scope: Some("profile".into()),
            }),
            None,
            None,
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let resp = create_project(
            State(state.clone()),
            None,
            None,
            Ok(Json(CreateProjectBody {
                path: other.to_string_lossy().to_string(),
                name: None,
                scope: Some("profile".into()),
                allow_override: false,
                default_base_branch: None,
                pinned: false,
                overrides: ProjectOverrides::default(),
            })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let names: Vec<String> = projects::load_profile("test")
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["p"], "neither write went through");
    }

    #[test]
    fn project_patch_parses_plugin_settings() {
        let parsed = parse_project_patch(&json!({
            "overrides": { "plugin_settings": { "acme.kit": { "depth": 3, "gone": null } } }
        }))
        .unwrap();
        let settings = &parsed.overrides.unwrap().plugin_settings["acme.kit"];
        assert_eq!(settings["depth"], Some(json!(3)));
        assert_eq!(settings["gone"], None);

        for bad in [
            json!({"overrides": {"plugin_settings": []}}),
            json!({"overrides": {"plugin_settings": {"acme.kit": 1}}}),
        ] {
            assert_eq!(
                parse_project_patch(&bad).unwrap_err().0,
                "bad_field",
                "{bad}"
            );
        }
    }

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
                    sandbox_enabled: None,
                    plugin_settings: Default::default(),
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
                    sandbox_enabled: None,
                    plugin_settings: Default::default(),
                }),
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {"sandbox_enabled": false}})),
            Ok(ProjectPatch {
                base_branch: None,
                pinned: None,
                overrides: Some(OverridesPatch {
                    worktree_enabled: None,
                    smart_rename: None,
                    sandbox_enabled: Some(Some(false)),
                    plugin_settings: Default::default(),
                }),
            })
        );
        assert_eq!(
            parse_project_patch(&json!({"overrides": {"sandbox_enabled": 1}})),
            Err((
                "bad_field",
                "overrides.sandbox_enabled must be a boolean or null"
            ))
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

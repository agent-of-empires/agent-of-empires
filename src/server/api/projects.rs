//! Canonical project reads and explicitly scoped registry commits.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::{api_error, AppState};
use crate::daemon::{
    CreateProjectBody, ProfileSnapshot, ProjectResponse, ReloadFailureCode, RuntimeHealth,
};
use crate::server::auth::{handler_elevated, AuthenticatedSession};
use crate::server::principal::{authorize, Capability, Principal};
use crate::session::config::settings_schema::validate_project_plugin_setting;
use crate::session::projects::{self, ProjectPatch, RegistryError};
use crate::session::{Project, ProjectScope};

impl From<Project> for ProjectResponse {
    fn from(project: Project) -> Self {
        Self {
            name: project.name,
            path: projects::canonical_key(project.path),
            scope: project.scope,
            default_base_branch: project.default_base_branch,
            pinned: project.pinned,
            overrides: project.overrides,
        }
    }
}

#[derive(Deserialize)]
pub struct ProjectQuery {
    pub scope: Option<String>,
    /// Profile whose registry entries to resolve; defaults to the served profile.
    #[serde(default)]
    pub profile: Option<String>,
}

fn project_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": code, "message": message})),
    )
        .into_response()
}

fn write_scope(scope: Option<&str>) -> Result<ProjectScope, Response> {
    match scope {
        Some("global") => Ok(ProjectScope::Global),
        Some("profile") => Ok(ProjectScope::Profile),
        _ => Err(project_error(
            StatusCode::BAD_REQUEST,
            "bad_scope",
            "An explicit global or profile scope is required",
        )),
    }
}

fn profile_index(profiles: &[ProfileSnapshot], name: Option<&str>) -> Result<usize, Response> {
    let name = name.ok_or_else(|| {
        project_error(
            StatusCode::BAD_REQUEST,
            "profile_required",
            "An explicit profile is required",
        )
    })?;
    profiles
        .iter()
        .position(|profile| profile.name == name)
        .ok_or_else(|| {
            project_error(
                StatusCode::NOT_FOUND,
                "profile_not_found",
                "Profile not found",
            )
        })
}

#[tracing::instrument(target = "http.api.projects", skip_all)]
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectQuery>,
) -> Response {
    if query
        .scope
        .as_deref()
        .is_some_and(|scope| scope != "global" && scope != "profile")
    {
        return project_error(
            StatusCode::BAD_REQUEST,
            "bad_scope",
            "Use global, profile, or omit the scope for merged projects",
        );
    }
    if let Some(profile) = query.profile.as_deref() {
        if let Err(message) = super::validate_profile_name(profile) {
            return project_error(StatusCode::BAD_REQUEST, "bad_profile", &message);
        }
    }
    let _namespace = state.profile_namespace.read().await;
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let contents = &snapshot.value.contents;
    if query.scope.as_deref() == Some("global") {
        return Json(&contents.global_projects).into_response();
    }
    let served = state.served_profile();
    let index = match profile_index(
        &contents.profiles,
        Some(query.profile.as_deref().unwrap_or(&served)),
    ) {
        Ok(index) => index,
        Err(response) => return response,
    };
    let profile = &contents.profiles[index];
    if query.scope.as_deref() == Some("profile") {
        return Json(&profile.projects).into_response();
    }
    Json(projects::merge_project_scopes(
        &contents.global_projects,
        &profile.projects,
        |project| project.path.as_str(),
    ))
    .into_response()
}

enum ProjectChange<T> {
    Current(T),
    Removed(ProjectResponse),
}

async fn commit_project(
    state: Arc<AppState>,
    scope: ProjectScope,
    profile: Option<String>,
    status: StatusCode,
    mutate: impl FnOnce(
            &str,
            ProjectScope,
        ) -> Result<projects::ProjectCommit<ProjectChange<usize>>, RegistryError>
        + Send
        + 'static,
) -> Response {
    let namespace = state.profile_namespace.read().await;
    let publication = state.publication.write().await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let selected_profile = if profile.is_some() || scope == ProjectScope::Profile {
        match profile_index(
            &state.canonical_metadata.read().await.profiles,
            profile.as_deref(),
        ) {
            Ok(index) => Some(index),
            Err(response) => return response,
        }
    } else {
        None
    };
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<_, RegistryError> {
            let commit = mutate(profile.as_deref().unwrap_or(""), scope)?;
            let metadata = worker_state.canonical_metadata.blocking_read();
            let shared_global = commit.target.same_target(&projects::open_registry(None)?)?;
            let mut shared_profiles = Vec::new();
            for (index, candidate) in metadata.profiles.iter().enumerate() {
                if commit
                    .target
                    .same_target(&projects::open_registry(Some(&candidate.name))?)?
                {
                    shared_profiles.push(index);
                }
            }
            if match scope {
                ProjectScope::Global => !shared_global,
                ProjectScope::Profile => {
                    !shared_profiles.contains(&selected_profile.expect("validated profile"))
                }
            } {
                return Err(
                    anyhow::anyhow!("project registry identity changed during commit").into(),
                );
            }
            Ok((
                commit.result,
                commit.effect,
                commit
                    .projects
                    .into_iter()
                    .map(ProjectResponse::from)
                    .collect::<Vec<_>>(),
                shared_global,
                shared_profiles,
            ))
        })();
        (profile, result)
    })
    .await;
    let (profile, result) = match result {
        Ok(result) => result,
        Err(error) => (None, Err(RegistryError::Other(error.into()))),
    };
    let (change, effect, mut committed, shared_global, shared_profiles) = match result {
        Ok(committed) => committed,
        Err(RegistryError::Conflict(message)) => {
            return project_error(StatusCode::CONFLICT, "conflict", &message)
        }
        Err(RegistryError::NotFound(message)) => {
            return project_error(StatusCode::NOT_FOUND, "not_found", &message)
        }
        Err(RegistryError::Forbidden(_)) => return elevation_required(),
        Err(RegistryError::Other(error)) => {
            tracing::error!(target: "http.api.projects", %error, "project registry commit failed");
            *state.canonical_health.write().await = RuntimeHealth::Degraded {
                code: ReloadFailureCode::Metadata,
                profiles: if scope == ProjectScope::Profile {
                    profile.into_iter().collect()
                } else {
                    Vec::new()
                },
            };
            state.runtime.request_publish();
            return project_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist the project registry",
            );
        }
    };
    let change = match change {
        ProjectChange::Current(index) => ProjectChange::Current(committed[index].path.clone()),
        ProjectChange::Removed(project) => ProjectChange::Removed(project),
    };
    {
        let mut metadata = state.canonical_metadata.write().await;
        match shared_profiles.split_last() {
            None => metadata.global_projects = committed,
            Some((&last, rest)) => {
                if shared_global {
                    let mut global = committed.clone();
                    if scope == ProjectScope::Profile {
                        for project in &mut global {
                            project.scope = ProjectScope::Global;
                        }
                    }
                    metadata.global_projects = global;
                }
                if scope == ProjectScope::Global {
                    for project in &mut committed {
                        project.scope = ProjectScope::Profile;
                    }
                }
                for &index in rest {
                    metadata.profiles[index].projects = committed.clone();
                }
                metadata.profiles[last].projects = committed;
            }
        }
    }
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.runtime.request_publish();
    drop(publication);
    drop(namespace);
    if let Some(effect) = &effect {
        emit_effective_changes(&state, effect).await;
    }
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let response = match change {
        ProjectChange::Removed(project) => (status, Json(project)).into_response(),
        ProjectChange::Current(path) => {
            let projects = match scope {
                ProjectScope::Global => Some(snapshot.value.contents.global_projects.as_slice()),
                ProjectScope::Profile => snapshot
                    .value
                    .contents
                    .profiles
                    .iter()
                    .find(|candidate| Some(candidate.name.as_str()) == profile.as_deref())
                    .map(|profile| profile.projects.as_slice()),
            };
            let Some(project) =
                projects.and_then(|projects| projects.iter().find(|project| project.path == path))
            else {
                return project_error(
                    StatusCode::CONFLICT,
                    "project_gone_after_commit",
                    "Project changed before its commit could be acknowledged",
                );
            };
            (status, Json(project)).into_response()
        }
    };
    crate::server::runtime::mutation_response(&snapshot.value.cursor, response)
}

#[tracing::instrument(target = "http.api.projects", skip_all)]
pub async fn create_project(
    State(state): State<Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<Principal>>,
    body: Result<Json<CreateProjectBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    body.overrides.plugin_settings.retain(|_, settings| {
        settings.retain(|_, value| !value.is_null());
        !settings.is_empty()
    });
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let scope = body.scope;
    let allow_override = body.allow_override;
    let preflight = tokio::task::spawn_blocking(move || {
        let profile = body.profile;
        let path = std::path::PathBuf::from(body.path);
        let canonical = path.canonicalize().unwrap_or(path);
        if !canonical.is_dir() {
            return Err(project_error(
                StatusCode::BAD_REQUEST,
                "not_a_directory",
                "Path does not exist or is not a directory",
            ));
        }
        let name = match body.name {
            Some(name) => name,
            None => {
                let base = canonical
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project".to_owned());
                projects::unique_name(&profile, scope, &base)
            }
        };
        let project = Project::new(name, canonical.to_string_lossy(), scope)
            .with_base_branch(body.default_base_branch)
            .with_pinned(body.pinned)
            .with_overrides(body.overrides);
        Ok((profile, project))
    })
    .await;
    let (profile, project) = match preflight {
        Ok(Ok(preflight)) => preflight,
        Ok(Err(response)) => return response,
        Err(error) => {
            tracing::error!(target: "http.api.projects", %error, "project preflight failed");
            return project_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "create_failed",
                "Failed to prepare the project",
            );
        }
    };
    let plugin_settings: PluginSettingsPatch = project
        .overrides
        .plugin_settings
        .iter()
        .map(|(plugin, settings)| {
            (
                plugin.clone(),
                settings
                    .iter()
                    .map(|(key, value)| (key.clone(), Some(value.clone())))
                    .collect(),
            )
        })
        .collect();
    let elevated = handler_elevated(
        &state,
        session.as_deref(),
        authorize(loopback.as_deref(), Capability::SkipElevation),
    )
    .await;
    if !elevated && !plugin_settings.values().all(BTreeMap::is_empty) {
        return elevation_required();
    }
    if let Err(message) = validate_plugin_settings(&plugin_settings) {
        return project_error(StatusCode::BAD_REQUEST, "bad_plugin_setting", &message);
    }
    commit_project(
        state,
        scope,
        Some(profile),
        StatusCode::CREATED,
        move |profile, scope| {
            projects::add_guarded(profile, scope, project, allow_override, |effect| {
                if elevated || effect.is_noop() {
                    Ok(())
                } else {
                    Err(RegistryError::Forbidden(PLUGIN_SETTINGS_ELEVATION.into()))
                }
            })
            .map(|commit| commit.map_result(ProjectChange::Current))
        },
    )
    .await
}

#[tracing::instrument(target = "http.api.projects", skip_all)]
pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ProjectQuery>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<Principal>>,
) -> Response {
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let scope = match write_scope(query.scope.as_deref()) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let elevated = handler_elevated(
        &state,
        session.as_deref(),
        authorize(loopback.as_deref(), Capability::SkipElevation),
    )
    .await;
    commit_project(
        state,
        scope,
        query.profile,
        StatusCode::OK,
        move |profile, scope| {
            projects::remove_guarded(profile, scope, &name, |entry, effect| {
                if elevated || (entry.overrides.plugin_settings.is_empty() && effect.is_noop()) {
                    Ok(())
                } else {
                    Err(RegistryError::Forbidden(PLUGIN_SETTINGS_ELEVATION.into()))
                }
            })
            .map(|commit| commit.map_result(|removed| ProjectChange::Removed(removed.into())))
        },
    )
    .await
}
#[derive(Default)]
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
        Some(serde_json::Value::Bool(value)) => Some(Some(*value)),
        Some(_) => {
            return Err((
                "bad_field",
                "overrides.worktree_enabled must be a boolean or null",
            ));
        }
    };
    let smart_rename = match body.get("smart_rename") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::Bool(value)) => Some(Some(*value)),
        Some(_) => {
            return Err((
                "bad_field",
                "overrides.smart_rename must be a boolean or null",
            ));
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
    mut body: serde_json::Value,
) -> Result<ProjectPatch, (&'static str, &'static str)> {
    let base_branch = match body
        .get_mut("default_base_branch")
        .map(serde_json::Value::take)
    {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(value)) => Some(Some(value)),
        Some(_) => return Err(("bad_field", "default_base_branch must be a string or null")),
    };
    let pinned = match body.get("pinned") {
        None => None,
        Some(serde_json::Value::Bool(value)) => Some(*value),
        Some(_) => return Err(("bad_field", "pinned must be a boolean")),
    };
    let overrides = match body.get("overrides") {
        None => OverridesPatch::default(),
        Some(value @ serde_json::Value::Object(_)) => parse_overrides_patch(value)?,
        Some(_) => return Err(("bad_field", "overrides must be an object")),
    };
    if base_branch.is_none() && pinned.is_none() && overrides.is_empty() {
        return Err((
            "no_fields",
            "provide at least one of: default_base_branch, pinned, overrides",
        ));
    }
    Ok(ProjectPatch {
        base_branch,
        pinned,
        worktree_enabled: overrides.worktree_enabled,
        smart_rename: overrides.smart_rename,
        sandbox_enabled: overrides.sandbox_enabled,
        plugin_settings: overrides.plugin_settings,
    })
}

#[tracing::instrument(target = "http.api.projects", skip_all)]
pub async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ProjectQuery>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<Principal>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let scope = match write_scope(query.scope.as_deref()) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let patch = match parse_project_patch(body) {
        Ok(patch) => patch,
        Err((code, message)) => return project_error(StatusCode::BAD_REQUEST, code, message),
    };
    if !patch.plugin_settings.values().all(BTreeMap::is_empty)
        && !handler_elevated(
            &state,
            session.as_deref(),
            authorize(loopback.as_deref(), Capability::SkipElevation),
        )
        .await
    {
        return elevation_required();
    }
    if let Err(message) = validate_plugin_settings(&patch.plugin_settings) {
        return project_error(StatusCode::BAD_REQUEST, "bad_plugin_setting", &message);
    }
    commit_project(
        state,
        scope,
        query.profile,
        StatusCode::OK,
        move |profile, scope| {
            projects::update(profile, scope, &name, patch)
                .map(|commit| commit.map_result(ProjectChange::Current))
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::parse_project_patch;
    use crate::session::ProjectOverrides;
    use serde_json::json;

    #[tokio::test]
    #[serial_test::serial]
    async fn plugin_setting_writes_need_elevation_but_other_overrides_do_not() {
        use super::*;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        projects::add(
            "test",
            ProjectScope::Global,
            Project::new("demo", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )
        .unwrap();
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
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
                    Query(ProjectQuery {
                        scope: Some("global".into()),
                        profile: Some("test".into()),
                    }),
                    Some(axum::Extension(AuthenticatedSession(session))),
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
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
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
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let ext = || Some(axum::Extension(AuthenticatedSession(session.clone())));
        let delete = |name: &str| {
            delete_project(
                State(state.clone()),
                Path(name.to_string()),
                Query(ProjectQuery {
                    scope: Some("global".into()),
                    profile: Some("test".into()),
                }),
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
                    profile: "test".into(),
                    scope: ProjectScope::Profile,
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
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
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
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let delete_shadow = || {
            delete_project(
                State(state.clone()),
                Path("shadow".to_string()),
                Query(ProjectQuery {
                    scope: Some("profile".into()),
                    profile: Some("test".into()),
                }),
                Some(axum::Extension(AuthenticatedSession(session.clone()))),
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
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let body: CreateProjectBody = serde_json::from_value(json!({
            "path": repo.to_string_lossy(),
            "profile": "test",
            "scope": "profile",
            "overrides": { "plugin_settings": { "acme.kit": { "verbose": null } } },
        }))
        .unwrap();

        let resp = create_project(
            State(state),
            Some(axum::Extension(AuthenticatedSession(session))),
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
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
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
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        // The global registry is the unreadable one; the writes below only touch the profile one.
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("projects.json"),
            "{ not json",
        )
        .unwrap();

        let resp = delete_project(
            State(state.clone()),
            Path("p".to_string()),
            Query(ProjectQuery {
                scope: Some("profile".into()),
                profile: Some("test".into()),
            }),
            None,
            None,
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(matches!(
            *state.canonical_health.read().await,
            RuntimeHealth::Degraded { .. }
        ));
        let resp = create_project(
            State(state.clone()),
            None,
            None,
            Ok(Json(CreateProjectBody {
                path: other.to_string_lossy().to_string(),
                name: None,
                profile: "test".into(),
                scope: ProjectScope::Profile,
                allow_override: false,
                default_base_branch: None,
                pinned: false,
                overrides: ProjectOverrides::default(),
            })),
        )
        .await
        .into_response();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "native mutations stay closed after the registry failure"
        );

        let names: Vec<String> = projects::load_profile("test")
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["p"], "neither write went through");
    }

    #[test]
    fn project_patch_parses_plugin_settings() {
        let parsed = parse_project_patch(json!({
            "overrides": { "plugin_settings": { "acme.kit": { "depth": 3, "gone": null } } }
        }))
        .unwrap();
        let settings = &parsed.plugin_settings["acme.kit"];
        assert_eq!(settings["depth"], Some(json!(3)));
        assert_eq!(settings["gone"], None);

        for bad in [
            json!({"overrides": {"plugin_settings": []}}),
            json!({"overrides": {"plugin_settings": {"acme.kit": 1}}}),
        ] {
            assert_eq!(
                parse_project_patch(bad.clone()).unwrap_err().0,
                "bad_field",
                "{bad}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shared_registry_commit_reflects_every_scope() -> anyhow::Result<()> {
        use crate::session::{projects, Project, ProjectScope, Storage};
        use axum::{body::Body, http::Request, routing::patch, Router};
        use tower::ServiceExt;

        let temp = tempfile::tempdir()?;
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let state = crate::server::test_support::build_test_app_state(Vec::new());

        projects::add(
            "alpha",
            ProjectScope::Global,
            Project::new(
                "shared",
                temp.path().to_string_lossy(),
                ProjectScope::Global,
            ),
            false,
        )?;
        for profile in ["alpha", "beta"] {
            Storage::new(profile, state.file_watch.clone())?;
            std::os::unix::fs::symlink(
                crate::session::get_app_dir()?.join("projects.json"),
                crate::session::get_profile_dir_path(profile)?.join("projects.json"),
            )?;
        }
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        state.runtime.publish(&state).await?;
        let router = Router::new()
            .route(
                "/api/projects/{name}",
                patch(super::update_project).delete(super::delete_project),
            )
            .with_state(state.clone());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/projects/shared?scope=profile&profile=alpha")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"pinned":true}"#))?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let snapshot = state.runtime.snapshot(&state).await?;
        assert!(snapshot.value.contents.global_projects[0].pinned);
        for name in ["alpha", "beta"] {
            let profile = snapshot
                .value
                .contents
                .profiles
                .iter()
                .find(|p| p.name == name)
                .unwrap();
            assert!(profile.projects[0].pinned);
            assert_eq!(profile.projects[0].scope, ProjectScope::Profile);
        }
        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/projects/shared?scope=global")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let snapshot = state.runtime.snapshot(&state).await?;
        assert!(snapshot.value.contents.global_projects.is_empty());
        for name in ["alpha", "beta"] {
            assert!(snapshot
                .value
                .contents
                .profiles
                .iter()
                .find(|p| p.name == name)
                .unwrap()
                .projects
                .is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn project_commit_publishes_its_complete_explicit_scope() -> anyhow::Result<()> {
        use crate::session::projects;
        use crate::session::{Project, ProjectScope, Storage};
        use axum::{
            body::Body,
            http::Request,
            routing::{get, patch},
            Router,
        };
        use tower::ServiceExt;
        let temp = tempfile::tempdir()?;
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let state = crate::server::test_support::build_test_app_state(Vec::new());

        for profile in ["alpha", "beta"] {
            Storage::new(profile, state.file_watch.clone())?;
            let path = temp.path().join(profile);
            std::fs::create_dir_all(&path)?;
            projects::add(
                profile,
                ProjectScope::Profile,
                Project::new("target", path.to_string_lossy(), ProjectScope::Profile),
                false,
            )?;
        }
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        state.runtime.publish(&state).await?;
        projects::add(
            "beta",
            ProjectScope::Profile,
            Project::new(
                "peer",
                temp.path().join("peer").to_string_lossy(),
                ProjectScope::Profile,
            ),
            false,
        )?;
        let router = Router::new()
            .route("/api/projects", get(super::list_projects))
            .route("/api/projects/{name}", patch(super::update_project))
            .with_state(state.clone());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/projects/target?scope=profile&profile=beta")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&projects::ProjectPatch {
                        base_branch: Some(Some(" release ".into())),
                        pinned: Some(true),
                        worktree_enabled: Some(Some(true)),
                        smart_rename: Some(Some(false)),
                        sandbox_enabled: Some(Some(true)),
                        plugin_settings: std::collections::BTreeMap::new(),
                    })?))?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(
            !projects::load_profile("alpha")?[0].pinned,
            "explicit profile mutated the default profile"
        );
        let published = state.runtime.snapshot(&state).await?;
        assert_eq!(
            response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER].to_str()?,
            published.value.cursor.epoch
        );
        assert_eq!(
            response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
                .to_str()?
                .parse::<u64>()?,
            published.value.cursor.revision
        );
        let profile = published
            .value
            .contents
            .profiles
            .iter()
            .find(|profile| profile.name == "beta")
            .unwrap();
        assert_eq!(
            profile
                .projects
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>(),
            ["target", "peer"]
        );
        assert!(profile.projects[0].pinned);
        assert_eq!(
            profile.projects[0].default_base_branch.as_deref(),
            Some("release")
        );
        assert_eq!(profile.projects[0].overrides.worktree_enabled, Some(true));
        assert_eq!(profile.projects[0].overrides.smart_rename, Some(false));
        assert_eq!(profile.projects[0].overrides.sandbox_enabled, Some(true));
        projects::add(
            "beta",
            ProjectScope::Profile,
            Project::new(
                "late",
                temp.path().join("late").to_string_lossy(),
                ProjectScope::Profile,
            ),
            false,
        )?;
        let listed = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/projects?scope=profile&profile=beta")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(listed.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(listed.into_body(), 16 * 1024 * 1024).await?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes)?,
            serde_json::to_value(&profile.projects)?
        );
        for (patch, expected_base, expected_smart_rename, expected_sandbox) in [
            (
                projects::ProjectPatch {
                    pinned: Some(false),
                    ..Default::default()
                },
                Some("release"),
                Some(false),
                Some(true),
            ),
            (
                projects::ProjectPatch {
                    base_branch: Some(None),
                    ..Default::default()
                },
                None,
                Some(false),
                Some(true),
            ),
            (
                projects::ProjectPatch {
                    smart_rename: Some(None),
                    ..Default::default()
                },
                None,
                None,
                Some(true),
            ),
            (
                projects::ProjectPatch {
                    sandbox_enabled: Some(Some(false)),
                    ..Default::default()
                },
                None,
                None,
                Some(false),
            ),
            (
                projects::ProjectPatch {
                    sandbox_enabled: Some(None),
                    ..Default::default()
                },
                None,
                None,
                None,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/api/projects/target?scope=profile&profile=beta")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&patch)?))?,
                )
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let receipt_epoch = response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER]
                .to_str()?
                .to_owned();
            let receipt_revision = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
                .to_str()?
                .parse::<u64>()?;
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
            let project: crate::daemon::ProjectResponse = serde_json::from_slice(&bytes)?;
            assert!(!project.pinned);
            assert_eq!(project.default_base_branch.as_deref(), expected_base);
            assert_eq!(project.overrides.worktree_enabled, Some(true));
            assert_eq!(project.overrides.smart_rename, expected_smart_rename);
            assert_eq!(project.overrides.sandbox_enabled, expected_sandbox);
            let persisted = projects::load_profile("beta")?;
            assert_eq!(persisted[0].overrides, project.overrides);
            let snapshot = state.runtime.snapshot(&state).await?;
            assert_eq!(receipt_epoch, snapshot.value.cursor.epoch);
            assert_eq!(receipt_revision, snapshot.value.cursor.revision);
            assert_eq!(
                projects::load_profile("alpha")?[0].overrides,
                crate::session::ProjectOverrides::default()
            );
            assert_eq!(
                persisted
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>(),
                ["target", "peer", "late"]
            );
            assert!(persisted[1..]
                .iter()
                .all(|p| !p.pinned && p.default_base_branch.is_none() && p.overrides.is_empty()));
            let profile = snapshot
                .value
                .contents
                .profiles
                .iter()
                .find(|p| p.name == "beta")
                .unwrap();
            assert_eq!(profile.projects[0].overrides, project.overrides);
        }
        let before = projects::load_profile("beta")?;
        for body in [
            r#"{"default_base_branch":"trunk","pinned":"false"}"#,
            r#"{"overrides":{"worktree_enabled":"yes"}}"#,
            r#"{"default_base_branch":"trunk","overrides":{"sandbox_enabled":1}}"#,
            r#"{"overrides":"nope"}"#,
            "{}",
        ] {
            let rejected = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/api/projects/target?scope=profile&profile=beta")
                        .header("content-type", "application/json")
                        .body(Body::from(body))?,
                )
                .await?;
            assert_eq!(rejected.status(), axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(
                serde_json::to_value(projects::load_profile("beta")?)?,
                serde_json::to_value(&before)?
            );
        }

        for (uri, status) in [
            (
                "/api/projects/target?scope=profile",
                axum::http::StatusCode::BAD_REQUEST,
            ),
            (
                "/api/projects/target?scope=profile&profile=missing",
                axum::http::StatusCode::NOT_FOUND,
            ),
        ] {
            let rejected = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"pinned":false}"#))?,
                )
                .await?;
            assert_eq!(rejected.status(), status);
        }
        assert!(!crate::session::get_profile_dir_path("missing")?.exists());
        Ok(())
    }
}

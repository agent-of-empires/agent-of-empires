//! Canonical project reads and explicitly scoped registry commits.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::AppState;
use crate::daemon::{
    CreateProjectBody, ProfileSnapshot, ProjectResponse, ReloadFailureCode, RuntimeHealth,
};
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
        }
    }
}

#[derive(Deserialize)]
pub struct ProjectQuery {
    pub scope: Option<String>,
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
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let contents = &snapshot.value.contents;
    if query.scope.as_deref() == Some("global") {
        return Json(&contents.global_projects).into_response();
    }
    let index = match profile_index(&contents.profiles, query.profile.as_deref()) {
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
    let (change, mut committed, shared_global, shared_profiles) = match result {
        Ok(committed) => committed,
        Err(RegistryError::Conflict(message)) => {
            return project_error(StatusCode::CONFLICT, "conflict", &message)
        }
        Err(RegistryError::NotFound(message)) => {
            return project_error(StatusCode::NOT_FOUND, "not_found", &message)
        }
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
    body: Result<Json<CreateProjectBody>, axum::extract::rejection::JsonRejection>,
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
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let scope = body.scope;
    let allow_override = body.allow_override;
    let preflight = tokio::task::spawn_blocking(move || {
        let path = std::path::PathBuf::from(body.path);
        let canonical = path.canonicalize().unwrap_or(path);
        if !canonical.is_dir() {
            return Err(project_error(
                StatusCode::BAD_REQUEST,
                "not_a_directory",
                "Path does not exist or is not a directory",
            ));
        }
        let name = body.name.unwrap_or_else(|| {
            canonical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_owned())
        });
        let project = Project::new(name, canonical.to_string_lossy(), scope)
            .with_base_branch(body.default_base_branch)
            .with_pinned(body.pinned);
        Ok((body.profile, project))
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
    commit_project(
        state,
        scope,
        Some(profile),
        StatusCode::CREATED,
        move |profile, scope| {
            projects::add(profile, scope, project, allow_override)
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
    commit_project(
        state,
        scope,
        query.profile,
        StatusCode::OK,
        move |profile, scope| {
            projects::remove(profile, scope, &name)
                .map(|commit| commit.map_result(|removed| ProjectChange::Removed(removed.into())))
        },
    )
    .await
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

#[tracing::instrument(target = "http.api.projects", skip_all)]
pub async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ProjectQuery>,
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
                    .body(Body::from(
                        r#"{"pinned":true,"default_base_branch":" release "}"#,
                    ))?,
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
        for (body, expected_base) in [
            (r#"{"pinned":false}"#, Some("release")),
            (r#"{"default_base_branch":null}"#, None),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/api/projects/target?scope=profile&profile=beta")
                        .header("content-type", "application/json")
                        .body(Body::from(body))?,
                )
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
            let project: crate::daemon::ProjectResponse = serde_json::from_slice(&bytes)?;
            assert!(!project.pinned);
            assert_eq!(project.default_base_branch.as_deref(), expected_base);
        }
        let before = projects::load_profile("beta")?;
        for body in [r#"{"default_base_branch":"trunk","pinned":"false"}"#, "{}"] {
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

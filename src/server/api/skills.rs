//! Skills management REST API.

use axum::extract::{FromRequestParts, Path};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::server::auth::{handler_elevated, AuthenticatedSession, LoopbackTrusted};
use crate::session::skills_model::{self, SkillError, SkillProvenance};

fn error_response(status: StatusCode, code: &str, message: String) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

fn skill_error(error: SkillError) -> Response {
    match error {
        SkillError::InvalidInput(message) => {
            error_response(StatusCode::BAD_REQUEST, "invalid_skill", message)
        }
        SkillError::NotFound(message) => {
            error_response(StatusCode::NOT_FOUND, "skill_not_found", message)
        }
        SkillError::Collision(message) => {
            error_response(StatusCode::CONFLICT, "skill_exists", message)
        }
        SkillError::ReadOnly(message) => {
            error_response(StatusCode::FORBIDDEN, "skill_read_only", message)
        }
        SkillError::Io(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{error:#}"),
        ),
    }
}

fn task_error(error: tokio::task::JoinError) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        error.to_string(),
    )
}

async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
    loopback_trusted: bool,
) -> Result<(), Response> {
    if state.read_only {
        return Err(super::read_only_response());
    }
    if let Some(response) = super::cityhall_block(state) {
        return Err(response);
    }
    if !handler_elevated(state, session, loopback_trusted).await {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue".to_string(),
        ));
    }
    Ok(())
}

/// Parts-only extractor that rejects forbidden mutations before Axum reads or
/// parses the request body.
pub struct SkillMutationGuard;

impl FromRequestParts<std::sync::Arc<AppState>> for SkillMutationGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let session = parts.extensions.get::<AuthenticatedSession>();
        let loopback_trusted = parts.extensions.get::<LoopbackTrusted>().is_some();
        mutation_gate(state, session, loopback_trusted).await?;
        Ok(Self)
    }
}

fn source_provenance(source: &str) -> Result<SkillProvenance, String> {
    if source == "aoe-managed" {
        return Ok(SkillProvenance::AoeManaged);
    }
    skills_model::skill_root(source)
        .map(|_| SkillProvenance::External {
            root: source.to_string(),
        })
        .ok_or_else(|| format!("Unknown skill source {source:?}"))
}

/// `GET /api/skills`: discover skills across every supported host root.
pub async fn list_skills() -> Response {
    match tokio::task::spawn_blocking(skills_model::discover_all).await {
        Ok(Ok(skills)) => Json(json!({
            "skills": skills.into_iter().map(|skill| {
                let provenance_label = skill.provenance.label();
                let writable = skill.provenance.is_writable();
                json!({
                    "directory": skill.directory,
                    "name": skill.name,
                    "description": skill.description,
                    "provenance": skill.provenance,
                    "provenanceLabel": provenance_label,
                    "writable": writable,
                })
            }).collect::<Vec<_>>(),
            "roots": skills_model::skill_roots(),
        }))
        .into_response(),
        Ok(Err(error)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{error:#}"),
        ),
        Err(error) => task_error(error),
    }
}

/// `GET /api/skills/{source}/{directory}`: read one source-qualified skill.
pub async fn read_skill(Path((source, directory)): Path<(String, String)>) -> Response {
    let provenance = match source_provenance(&source) {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_skill_source", message)
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| {
            SkillError::Io(anyhow::anyhow!("could not resolve home dir for skills"))
        })?;
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        skills_model::read_skill(&home, &app_dir, &provenance, &directory)
    })
    .await;
    match result {
        Ok(Ok(skill)) => Json(skill).into_response(),
        Ok(Err(error)) => skill_error(error),
        Err(error) => task_error(error),
    }
}

#[derive(Deserialize)]
pub struct CreateSkillBody {
    directory: String,
    description: Option<String>,
}

/// `POST /api/skills`: create a new AoE-managed skill.
pub async fn create_skill(
    _guard: SkillMutationGuard,
    Json(body): Json<CreateSkillBody>,
) -> Response {
    let directory = body.directory;
    let response_directory = directory.clone();
    let result = tokio::task::spawn_blocking(move || {
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        skills_model::create_skill(&app_dir, &directory, body.description.as_deref())
    })
    .await;
    mutation_response(result, Some(response_directory))
}

#[derive(Deserialize)]
pub struct EditSkillBody {
    content: String,
}

/// `PUT /api/skills/{directory}`: edit an AoE-managed skill.
pub async fn edit_skill(
    _guard: SkillMutationGuard,
    Path(directory): Path<String>,
    Json(body): Json<EditSkillBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| {
            SkillError::Io(anyhow::anyhow!("could not resolve home dir for skills"))
        })?;
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        skills_model::edit_skill(&home, &app_dir, &directory, &body.content)
    })
    .await;
    mutation_response(result, None)
}

/// `DELETE /api/skills/{directory}`: delete an AoE-managed skill.
pub async fn delete_skill(_guard: SkillMutationGuard, Path(directory): Path<String>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| {
            SkillError::Io(anyhow::anyhow!("could not resolve home dir for skills"))
        })?;
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        skills_model::delete_skill(&home, &app_dir, &directory)
    })
    .await;
    mutation_response(result, None)
}

#[derive(Default, Deserialize)]
pub struct AdoptSkillBody {
    destination: Option<String>,
}

/// `POST /api/skills/{source}/{directory}/adopt`: copy an external skill into
/// the AoE-managed store.
pub async fn adopt_skill(
    _guard: SkillMutationGuard,
    Path((source, directory)): Path<(String, String)>,
    Json(body): Json<AdoptSkillBody>,
) -> Response {
    let provenance = match source_provenance(&source) {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_skill_source", message)
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| {
            SkillError::Io(anyhow::anyhow!("could not resolve home dir for skills"))
        })?;
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        skills_model::adopt_skill(
            &home,
            &app_dir,
            &provenance,
            &directory,
            body.destination.as_deref(),
        )
    })
    .await;
    match result {
        Ok(Ok(directory)) => (
            StatusCode::CREATED,
            Json(json!({ "ok": true, "directory": directory })),
        )
            .into_response(),
        Ok(Err(error)) => skill_error(error),
        Err(error) => task_error(error),
    }
}

#[derive(Default, Deserialize)]
pub struct SyncSkillsBody {
    /// Roots to reconcile. Omitted or empty means every known root, which is
    /// what "share with all agents" does.
    #[serde(default)]
    roots: Vec<String>,
    /// Skills the caller has explicitly asked AoE to take over, overwriting a
    /// skill AoE does not manage or a propagated copy edited in place. Empty
    /// means overwrite nothing, which is the default and what every automatic
    /// sync uses.
    #[serde(default)]
    replace: Vec<String>,
    /// When non-empty, reconcile only these skills. This is what makes sharing
    /// a single skill a single-skill operation rather than a full sync whose
    /// report is filtered afterwards.
    #[serde(default)]
    directories: Vec<String>,
}

/// `POST /api/skills/sync`: reconcile the managed store into agent skills dirs.
///
/// Returns one outcome per skill per root rather than failing on the first
/// conflict: a destination AoE does not own is a normal result the user needs to
/// see, not an error that should abandon the remaining roots.
pub async fn sync_skills(_guard: SkillMutationGuard, Json(body): Json<SyncSkillsBody>) -> Response {
    for root in &body.roots {
        if skills_model::skill_root(root).is_none() {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_skill_source",
                format!("Unknown skill root {root:?}"),
            );
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| {
            SkillError::Io(anyhow::anyhow!("could not resolve home dir for skills"))
        })?;
        let app_dir = crate::session::get_app_dir().map_err(SkillError::Io)?;
        let options = skills_model::SyncOptions {
            replace: body.replace.into_iter().collect(),
            only: body.directories.into_iter().collect(),
        };
        if body.roots.is_empty() {
            Ok(skills_model::sync_all_roots(&home, &app_dir, &options))
        } else {
            let mut out = Vec::new();
            for root in &body.roots {
                out.extend(skills_model::sync_root(&home, &app_dir, root, &options)?);
            }
            Ok(out)
        }
    })
    .await;
    match result {
        Ok(Ok(outcomes)) => Json(json!({ "ok": true, "outcomes": outcomes })).into_response(),
        Ok(Err(error)) => skill_error(error),
        Err(error) => task_error(error),
    }
}

fn mutation_response(
    result: Result<Result<(), SkillError>, tokio::task::JoinError>,
    directory: Option<String>,
) -> Response {
    match result {
        Ok(Ok(())) => Json(json!({ "ok": true, "directory": directory })).into_response(),
        Ok(Err(error)) => skill_error(error),
        Err(error) => task_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_and_domain_errors_map_to_api_contract() {
        assert_eq!(
            source_provenance("claude-user").unwrap(),
            SkillProvenance::External {
                root: "claude-user".to_string()
            }
        );
        assert!(source_provenance("unknown").is_err());

        let cases = [
            (
                SkillError::InvalidInput("bad".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                SkillError::NotFound("missing".into()),
                StatusCode::NOT_FOUND,
            ),
            (SkillError::Collision("exists".into()), StatusCode::CONFLICT),
            (
                SkillError::ReadOnly("readonly".into()),
                StatusCode::FORBIDDEN,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(skill_error(error).status(), expected);
        }
    }
}

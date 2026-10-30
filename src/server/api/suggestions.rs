//! `/api/suggestions`: snapshot and user-owned actions on plugin suggestions.
//!
//! Nothing here creates a session; `prepare` only resolves the wizard prefill.
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::{api_error, AppState};
use crate::plugin::suggestions::{self, Provider};
use axum::http::StatusCode;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRef {
    provider_id: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBody {
    provider_id: String,
    criterion: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateKey {
    provider_id: String,
    criterion: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateBody {
    provider_id: String,
    criterion: String,
    template: String,
}

#[derive(Deserialize)]
pub struct TemplateQuery {
    provider_id: Option<String>,
    criterion: Option<String>,
}

fn unavailable() -> axum::response::Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "Plugin host is unavailable",
    )
}

fn internal(error: impl std::fmt::Display) -> axum::response::Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        error.to_string(),
    )
}

fn unknown_provider() -> axum::response::Response {
    api_error(
        StatusCode::NOT_FOUND,
        "unknown_provider",
        "No active plugin provides suggestions under that id",
    )
}

fn stale_item() -> axum::response::Response {
    api_error(
        StatusCode::CONFLICT,
        "stale_suggestion",
        "Suggestion is no longer published; refresh and retry",
    )
}

fn find_provider(id: &str) -> Option<Provider> {
    suggestions::providers().into_iter().find(|p| p.id == id)
}

pub async fn get_suggestions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(host) = state.plugin_host.clone() else {
        return Json(json!({ "providers": [], "items": [] })).into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        let providers = suggestions::providers();
        host.with_suggestions_db(|conn, profile| suggestions::snapshot(conn, profile, &providers))
    })
    .await;
    match result {
        Ok(Ok(snapshot)) => Json(snapshot).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn refresh_suggestions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Some(host) = &state.plugin_host else {
        return Json(json!({ "ok": true, "notified": 0 })).into_response();
    };
    let notified = host.refresh_suggestions(None).await;
    Json(json!({ "ok": true, "notified": notified })).into_response()
}

async fn set_dismissed(
    state: Arc<AppState>,
    item: ItemRef,
    dismissed: bool,
) -> axum::response::Response {
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let Some(provider) = find_provider(&item.provider_id) else {
        return unknown_provider();
    };
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| {
            suggestions::set_dismissed(conn, profile, &provider, &item.id, dismissed)
        })
    })
    .await;
    match result {
        Ok(Ok(true)) => Json(json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => stale_item(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn dismiss_suggestion(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ItemRef>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(item) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    set_dismissed(state, item, true).await
}

pub async fn undismiss_suggestion(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ItemRef>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(item) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    set_dismissed(state, item, false).await
}

pub async fn update_suggestion_source(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SourceBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let Some(provider) = find_provider(&body.provider_id) else {
        return unknown_provider();
    };
    if !provider
        .spec
        .sources
        .iter()
        .any(|s| s.criterion == body.criterion)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "The provider does not declare that criterion",
        );
    }
    let stored = {
        let (host, provider_id, criterion) = (
            host.clone(),
            body.provider_id.clone(),
            body.criterion.clone(),
        );
        tokio::task::spawn_blocking(move || {
            host.with_suggestions_db(|conn, profile| {
                suggestions::set_source(conn, profile, &provider_id, &criterion, body.enabled)
            })
        })
        .await
    };
    match stored {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return internal(error),
        Err(error) => return internal(error),
    }
    host.refresh_suggestions(Some(&body.provider_id)).await;
    Json(json!({
        "provider_id": body.provider_id,
        "criterion": body.criterion,
        "enabled": body.enabled,
    }))
    .into_response()
}

pub async fn prepare_suggestion(
    State(state): State<Arc<AppState>>,
    Json(item): Json<ItemRef>,
) -> impl IntoResponse {
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let Some(provider) = find_provider(&item.provider_id) else {
        return unknown_provider();
    };
    let result = tokio::task::spawn_blocking(move || {
        let (profile, found, template) = host.with_suggestions_db(|conn, profile| {
            let found = suggestions::find_item(conn, profile, &provider, &item.id)?;
            let template = match &found {
                Some(found) => {
                    suggestions::template(conn, profile, &provider.id, &found.criterion)?
                }
                None => None,
            };
            Ok::<_, anyhow::Error>((profile.to_string(), found, template))
        })?;
        // Config and project resolution touch disk, so they run off the lock.
        Ok::<_, anyhow::Error>(
            found.map(|found| suggestions::prepare(&profile, &found, template.as_deref())),
        )
    })
    .await;
    match result {
        Ok(Ok(Some(prefill))) => Json(prefill).into_response(),
        Ok(Ok(None)) => stale_item(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

/// The provider, or the error response when it is unknown or does not
/// declare `criterion`.
fn declared_source(
    provider_id: &str,
    criterion: &str,
) -> Result<Provider, axum::response::Response> {
    let provider = find_provider(provider_id).ok_or_else(unknown_provider)?;
    if provider
        .spec
        .sources
        .iter()
        .any(|s| s.criterion == criterion)
    {
        Ok(provider)
    } else {
        Err(api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "The provider does not declare that criterion",
        ))
    }
}

fn template_json(
    provider_id: &str,
    criterion: &str,
    template: Option<String>,
) -> serde_json::Value {
    json!({
        "provider_id": provider_id,
        "criterion": criterion,
        "default": template.is_none(),
        "template": template,
    })
}

pub async fn get_suggestion_templates(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TemplateQuery>,
) -> impl IntoResponse {
    let key = match (query.provider_id, query.criterion) {
        (None, None) => None,
        (Some(provider_id), Some(criterion)) => {
            if let Err(response) = declared_source(&provider_id, &criterion) {
                return response;
            }
            Some((provider_id, criterion))
        }
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                "Pass both provider_id and criterion, or neither",
            )
        }
    };
    let Some(host) = state.plugin_host.clone() else {
        return match key {
            None => Json(json!({ "templates": [] })).into_response(),
            Some(_) => unavailable(),
        };
    };
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| match key {
            Some((provider_id, criterion)) => {
                suggestions::template(conn, profile, &provider_id, &criterion)
                    .map(|t| template_json(&provider_id, &criterion, t))
            }
            None => suggestions::templates(conn, profile).map(|rows| {
                json!({ "templates": rows.into_iter()
                    .map(|(provider_id, criterion, template)| {
                        json!({ "provider_id": provider_id, "criterion": criterion, "template": template })
                    })
                    .collect::<Vec<_>>() })
            }),
        })
    })
    .await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn put_suggestion_template(
    State(state): State<Arc<AppState>>,
    body: Result<Json<TemplateBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    if let Err(response) = declared_source(&body.provider_id, &body.criterion) {
        return response;
    }
    if body.template.len() > suggestions::MAX_PROMPT_BYTES {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            format!(
                "Template is limited to {} bytes",
                suggestions::MAX_PROMPT_BYTES
            ),
        );
    }
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| {
            suggestions::set_template(
                conn,
                profile,
                &body.provider_id,
                &body.criterion,
                &body.template,
            )
            .map(|()| {
                template_json(
                    &body.provider_id,
                    &body.criterion,
                    Some(body.template.clone()),
                )
            })
        })
    })
    .await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn delete_suggestion_template(
    State(state): State<Arc<AppState>>,
    body: Result<Json<TemplateKey>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(key) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    if let Err(response) = declared_source(&key.provider_id, &key.criterion) {
        return response;
    }
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| {
            suggestions::delete_template(conn, profile, &key.provider_id, &key.criterion)
        })
    })
    .await;
    match result {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

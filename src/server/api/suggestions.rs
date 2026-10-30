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
pub struct MappingBody {
    provider_id: String,
    mapping_key: String,
    projects: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRef {
    provider_id: String,
    mapping_key: String,
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
        let (profile, found) = host.with_suggestions_db(|conn, profile| {
            suggestions::find_item_with_mapping(conn, profile, &provider, &item.id)
                .map(|found| (profile.to_string(), found))
        })?;
        // Config and project resolution touch disk, so they run off the lock.
        Ok::<_, anyhow::Error>(
            found.map(|(found, mapped)| suggestions::prepare(&profile, &found, mapped.as_deref())),
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

pub async fn list_suggestion_mappings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let result =
        tokio::task::spawn_blocking(move || host.with_suggestions_db(suggestions::list_mappings))
            .await;
    match result {
        Ok(Ok(mappings)) => Json(mappings).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn put_suggestion_mapping(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MappingBody>, axum::extract::rejection::JsonRejection>,
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
    if find_provider(&body.provider_id).is_none() {
        return unknown_provider();
    }
    let result = tokio::task::spawn_blocking(move || {
        let profile = host.suggestions_profile();
        let registered = crate::session::projects::load_merged(&profile)?;
        let projects =
            match suggestions::normalize_mapping(&body.mapping_key, &body.projects, &registered) {
                Ok(projects) => projects,
                Err(message) => return Ok(Err(message)),
            };
        host.with_suggestions_db(|conn, profile| {
            suggestions::put_mapping(
                conn,
                profile,
                &body.provider_id,
                &body.mapping_key,
                &projects,
            )
        })?;
        Ok::<_, anyhow::Error>(Ok(json!({
            "provider_id": body.provider_id,
            "mapping_key": body.mapping_key,
            "projects": projects,
        })))
    })
    .await;
    match result {
        Ok(Ok(Ok(stored))) => Json(stored).into_response(),
        Ok(Ok(Err(message))) => api_error(StatusCode::BAD_REQUEST, "validation_failed", message),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn delete_suggestion_mapping(
    State(state): State<Arc<AppState>>,
    query: Result<Query<MappingRef>, axum::extract::rejection::QueryRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Query(target) = match query {
        Ok(query) => query,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| {
            suggestions::delete_mapping(conn, profile, &target.provider_id, &target.mapping_key)
        })
    })
    .await;
    match result {
        Ok(Ok(deleted)) => Json(json!({ "ok": true, "deleted": deleted })).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

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
use crate::plugin::suggestions::{self, PrepareRefusal, Provider};
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
        return Json(json!({
            "providers": [],
            "items": [],
            "prefs": suggestions::prefs_json(suggestions::DEFAULT_PREFS, &[]),
        }))
        .into_response();
    };
    let profile = host.suggestions_profile();
    let started = suggestions::started_sessions(&state.instances.read().await, &profile);
    let refreshing = host.refreshing_suggestions();
    let result = tokio::task::spawn_blocking(move || {
        let providers = suggestions::providers();
        // Disk reads stay off the store lock.
        let registered = crate::session::projects::load_merged(&profile).unwrap_or_default();
        host.with_suggestions_db(|conn, profile| {
            let mut snapshot = suggestions::snapshot(conn, profile, &providers)?;
            suggestions::attach_projects(conn, profile, &mut snapshot, &registered)?;
            Ok::<_, anyhow::Error>(snapshot)
        })
    })
    .await;
    match result {
        Ok(Ok(mut snapshot)) => {
            suggestions::attach_started(&mut snapshot, &started);
            suggestions::attach_refreshing(&mut snapshot, &refreshing);
            Json(snapshot).into_response()
        }
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

pub async fn run_suggestion_checks(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Some(host) = &state.plugin_host else {
        return Json(json!({ "ok": true, "notified": 0 })).into_response();
    };
    let notified = host.run_suggestion_checks().await;
    Json(json!({ "ok": true, "notified": notified })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRef {
    provider_id: String,
}

pub async fn run_suggestion_fix(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ProviderRef>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = &state.plugin_host else {
        return unavailable();
    };
    if find_provider(&body.provider_id).is_none() {
        return unknown_provider();
    }
    match host.run_suggestion_fix(&body.provider_id).await {
        Ok(Some(notified)) => {
            Json(json!({ "ok": true, "notified": usize::from(notified) })).into_response()
        }
        Ok(None) => api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "The provider has no fix for its current error",
        ),
        Err(error) => internal(error),
    }
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

pub async fn patch_suggestion_prefs(
    State(state): State<Arc<AppState>>,
    body: Result<Json<suggestions::PrefsPatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(patch) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let result = tokio::task::spawn_blocking({
        let host = host.clone();
        move || {
            let known: Vec<String> = suggestions::providers().into_iter().map(|p| p.id).collect();
            host.with_suggestions_db(|conn, profile| {
                suggestions::apply_prefs(conn, profile, &known, &patch)
            })
        }
    })
    .await;
    match result {
        Ok(Ok(Ok(refresh_changed))) => {
            if refresh_changed {
                host.refresh_suggestions(None).await;
            }
            Json(json!({ "ok": true })).into_response()
        }
        Ok(Ok(Err(message))) => api_error(StatusCode::BAD_REQUEST, "validation_failed", message),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
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
            let found = suggestions::find_item_with_mapping(conn, profile, &provider, &item.id)?;
            let template = match &found {
                Some((found, _)) => {
                    suggestions::template(conn, profile, &provider.id, &found.criterion)?
                }
                None => None,
            };
            Ok::<_, anyhow::Error>((profile.to_string(), found, template))
        })?;
        // Config and project resolution touch disk, so they run off the lock.
        Ok::<_, anyhow::Error>(found.map(|(found, mapped)| {
            suggestions::prepare(&profile, &found, template.as_deref(), mapped.as_deref())
        }))
    })
    .await;
    match result {
        Ok(Ok(Some(Ok(prefill)))) => Json(prefill).into_response(),
        Ok(Ok(Some(Err(PrepareRefusal::Stale)))) => api_error(
            StatusCode::BAD_REQUEST,
            "stale",
            "That suggestion is no longer open at its source",
        ),
        Ok(Ok(Some(Err(PrepareRefusal::PromptTooLong)))) => api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            format!(
                "The expanded prompt exceeds {} bytes",
                suggestions::MAX_PROMPT_BYTES
            ),
        ),
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
    check_declared(find_provider(provider_id), criterion)
}

fn check_declared(
    provider: Option<Provider>,
    criterion: &str,
) -> Result<Provider, axum::response::Response> {
    let provider = provider.ok_or_else(unknown_provider)?;
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

/// What a PUT must satisfy before anything is stored.
fn check_template_put(
    provider: Option<Provider>,
    criterion: &str,
    template: &str,
) -> Result<(), axum::response::Response> {
    check_declared(provider, criterion)?;
    if template.len() > suggestions::MAX_PROMPT_BYTES {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            format!(
                "Template is limited to {} bytes",
                suggestions::MAX_PROMPT_BYTES
            ),
        ));
    }
    Ok(())
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
    if let Err(response) = check_template_put(
        find_provider(&body.provider_id),
        &body.criterion,
        &body.template,
    ) {
        return response;
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
    // No provider or criterion check: a listed template must stay deletable
    // after its plugin is disabled or drops the criterion.
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

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::{SuggestionSource, SuggestionsSpec};

    fn provider() -> Provider {
        Provider {
            id: "acme.github".into(),
            name: "GitHub".into(),
            spec: SuggestionsSpec {
                sources: vec![SuggestionSource {
                    criterion: "assigned".into(),
                    label: "Assigned".into(),
                    description: String::new(),
                    variables: vec![],
                    default_enabled: true,
                }],
                uses_agent: false,
            },
        }
    }

    #[test]
    fn template_put_is_bounded_and_needs_a_declared_source() {
        let max = suggestions::MAX_PROMPT_BYTES;
        let status = |provider: Option<Provider>, criterion: &str, len: usize| {
            check_template_put(provider, criterion, &"x".repeat(len))
                .err()
                .map(|r| r.status())
        };
        assert_eq!(status(Some(provider()), "assigned", 0), None);
        assert_eq!(status(Some(provider()), "assigned", max), None);
        assert_eq!(
            status(Some(provider()), "assigned", max + 1),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            status(Some(provider()), "undeclared", 1),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(status(None, "assigned", 1), Some(StatusCode::NOT_FOUND));
    }
}

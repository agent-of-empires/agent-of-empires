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
use crate::plugin::suggestions::{self, PrepareRefusal, Preset, Provider};
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
pub struct PresetKey {
    provider_id: String,
    criterion: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBody {
    provider_id: String,
    criterion: String,
    preset: Preset,
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
    let refreshing = host.refreshing_suggestions();
    let result = tokio::task::spawn_blocking(move || {
        let providers = suggestions::providers();
        // Disk reads stay off the store lock.
        let registered = crate::session::projects::load_merged(&profile)?;
        let (mut snapshot, mappings, links, presets) =
            host.with_suggestions_db(|conn, profile| {
                Ok::<_, anyhow::Error>((
                    suggestions::snapshot(conn, profile, &providers)?,
                    suggestions::load_mappings(conn, profile)?,
                    suggestions::settled_links(conn, profile)?,
                    suggestions::presets(conn, profile)?,
                ))
            })?;
        // Canonicalizing paths hits the filesystem: keep it off the store lock.
        suggestions::attach_projects(&mappings, &mut snapshot, &registered);
        suggestions::attach_presets(&profile, &presets, &registered, &mut snapshot);
        Ok::<_, anyhow::Error>((snapshot, links))
    })
    .await;
    match result {
        Ok(Ok((mut snapshot, links))) => {
            let started = {
                let instances = state.instances.read().await;
                // Same visibility as the CityHall session list: terminal sessions stay hidden.
                suggestions::started_sessions(
                    &links,
                    instances
                        .iter()
                        .filter(|inst| !state.cityhall_mode || inst.is_structured()),
                )
            };
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

/// The prefill of item `id`: `None` when it is no longer published.
fn prefill_for(
    host: &crate::plugin::host::PluginHost,
    provider: &Provider,
    id: &str,
) -> anyhow::Result<Option<Result<serde_json::Value, PrepareRefusal>>> {
    let (profile, found, preset) = host.with_suggestions_db(|conn, profile| {
        let found = suggestions::find_item_with_mapping(conn, profile, provider, id)?;
        let preset = match &found {
            Some((found, _)) => suggestions::preset(conn, profile, &provider.id, &found.criterion)?,
            None => None,
        };
        Ok::<_, anyhow::Error>((profile.to_string(), found, preset))
    })?;
    // Config and project resolution touch disk, so they run off the lock.
    Ok(found.map(|(found, mapped)| {
        suggestions::prepare(&profile, &found, preset.as_ref(), mapped.as_deref())
    }))
}

fn refusal_response(refusal: PrepareRefusal) -> axum::response::Response {
    match refusal {
        PrepareRefusal::Stale => api_error(
            StatusCode::BAD_REQUEST,
            "stale",
            "That suggestion is no longer open at its source",
        ),
        PrepareRefusal::PromptTooLong => api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            format!(
                "The expanded prompt exceeds {} bytes",
                crate::session::MAX_INITIAL_PROMPT_BYTES
            ),
        ),
        PrepareRefusal::Registry(error) => internal(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartProject {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartBody {
    provider_id: String,
    id: String,
    /// Link the item's repo to this Project first, then start.
    #[serde(default)]
    project: Option<StartProject>,
    #[serde(default)]
    approve_hooks: bool,
    /// The fingerprint of the `hooks_need_trust` answer being approved.
    #[serde(default)]
    review_fingerprint: Option<crate::daemon::CreationTrustFingerprint>,
}

fn start_failed(reason: impl Into<String>, hint: &str) -> axum::response::Response {
    let reason = reason.into();
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "error": "start_failed",
            "message": reason,
            "reason": reason,
            "hint": hint,
            "retryable": true,
        })),
    )
        .into_response()
}

/// Saves `path` as the Project of the item's repo. `Err` is a user-facing refusal.
fn link_project(
    host: &crate::plugin::host::PluginHost,
    provider: &Provider,
    id: &str,
    path: &str,
) -> anyhow::Result<Result<(), String>> {
    let registered = crate::session::projects::load_merged(&host.suggestions_profile())?;
    host.with_suggestions_db(|conn, profile| {
        let Some((item, _)) = suggestions::find_item_with_mapping(conn, profile, provider, id)?
        else {
            return Ok(Err("That suggestion is no longer published".to_string()));
        };
        let Some(key) = item.mapping_key else {
            return Ok(Err("This suggestion names no repo to link".to_string()));
        };
        let projects = match suggestions::normalize_mapping(&key, &[path.to_string()], &registered)
        {
            Ok(projects) => projects,
            Err(message) => return Ok(Err(message)),
        };
        suggestions::put_mapping(conn, profile, &provider.id, &key, &projects)?;
        Ok(Ok(()))
    })
}

/// 409 asking the caller to approve what the repo's hooks and MCP servers will run.
fn hooks_need_trust(review: &super::sessions::StartTrust) -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "hooks_need_trust",
            "message": "Repository hooks require approval before this Session starts",
            "commands": {
                "on_create": review.merged_hooks.on_create,
                "on_launch": review.merged_hooks.on_launch,
                "on_destroy": review.merged_hooks.on_destroy,
            },
            "needs_mcp_trust": review.mcp_need_trust,
            "mcp_summaries": review.mcp_summaries,
            "review_fingerprint": review.fingerprint,
        })),
    )
        .into_response()
}

/// Turns the answer of the create into Start's contract. `profile` and `path`
/// re-read the review when the repo's trust moved under the create.
async fn start_outcome(
    response: axum::response::Response,
    profile: String,
    path: String,
) -> axum::response::Response {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let code = body["error"].as_str().unwrap_or_default();
    if status == StatusCode::CREATED {
        return (
            StatusCode::CREATED,
            Json(json!({ "session_id": body["id"], "title": body["title"] })),
        )
            .into_response();
    }
    if status == StatusCode::CONFLICT && matches!(code, "already_started" | "stale_suggestion") {
        return (status, Json(body)).into_response();
    }
    if code == "creation_trust_changed" {
        return api_error(
            StatusCode::CONFLICT,
            "trust_changed",
            "The repository's hooks changed after you reviewed them; review them again",
        );
    }
    if code == "hooks_need_trust" {
        let review = tokio::task::spawn_blocking(move || {
            super::sessions::review_start_trust(&profile, std::path::Path::new(&path))
        })
        .await;
        return match review {
            Ok(Ok(review)) => hooks_need_trust(&review),
            Ok(Err(error)) => start_failed(
                error.to_string(),
                "Check the Project's folder, then try again.",
            ),
            Err(error) => internal(error),
        };
    }
    let hint = match code {
        "acp_adapter_missing" | "agent_not_allowed" => {
            "Choose another agent in the preset, then try again."
        }
        _ => "Try again, or use Start with changes to see the full error.",
    };
    let reason = body["message"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("The Session could not be created ({status})"));
    start_failed(reason, hint)
}

pub async fn start_suggestion(
    State(state): State<Arc<AppState>>,
    body: Result<Json<StartBody>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
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
    let StartBody {
        provider_id,
        id,
        project,
        approve_hooks,
        review_fingerprint,
    } = body;

    let prefill = {
        let (host, id) = (host.clone(), id.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(project) = project {
                if let Err(message) = link_project(&host, &provider, &id, &project.path)? {
                    return Ok(Err(api_error(
                        StatusCode::BAD_REQUEST,
                        "validation_failed",
                        message,
                    )));
                }
            }
            prefill_for(&host, &provider, &id).map(Ok)
        })
        .await
    };
    let prefill = match prefill {
        Ok(Ok(Ok(Some(Ok(prefill))))) => prefill,
        Ok(Ok(Ok(Some(Err(refusal))))) => return refusal_response(refusal),
        Ok(Ok(Ok(None))) => return stale_item(),
        Ok(Ok(Err(response))) => return response,
        Ok(Err(error)) => return internal(error),
        Err(error) => return internal(error),
    };
    let Some(path) = prefill["project"]["path"].as_str().map(String::from) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "no_project",
                "message": "This suggestion's repo is not linked to a Project",
                "hint": "link_project",
            })),
        )
            .into_response();
    };

    // The create gate still refuses unapproved hooks; this tells the caller what to approve.
    let profile = state.served_profile().to_string();
    let (review_profile, review_path) = (profile.clone(), path.clone());
    let review = tokio::task::spawn_blocking(move || {
        super::sessions::review_start_trust(&review_profile, std::path::Path::new(&review_path))
    })
    .await;
    let review = match review {
        Ok(Ok(review)) => review,
        Ok(Err(error)) => {
            return start_failed(
                error.to_string(),
                "Check the Project's folder, then try again.",
            )
        }
        Err(error) => return internal(error),
    };
    let (mut trust_hooks, mut trust_review) = (None, None);
    if review.hooks_need_trust || review.mcp_need_trust {
        if !approve_hooks {
            return hooks_need_trust(&review);
        }
        let Some(fingerprint) = review_fingerprint else {
            return api_error(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                "approve_hooks needs the review_fingerprint of the hooks it approves",
            );
        };
        (trust_hooks, trust_review) = (Some(true), Some(fingerprint));
    }

    let prompt = prefill["initial_prompt"]
        .as_str()
        .filter(|prompt| !prompt.is_empty());
    let create = serde_json::from_value::<crate::daemon::CreateSessionBody>(json!({
        "title": prefill["title"],
        "path": path,
        "tool": prefill["tool"],
        "view": prefill["view"],
        "yolo_mode": prefill["yolo"],
        "worktree_enabled": prefill["worktree"],
        "sandbox": prefill["sandbox"],
        "initial_prompt": prompt,
        "suggestion": { "provider_id": provider_id, "id": id },
        "trust_hooks": trust_hooks,
        "trust_review": trust_review,
    }));
    let create = match create {
        Ok(create) => create,
        Err(error) => return internal(error),
    };
    let reference = uuid::Uuid::new_v4().to_string();
    let response =
        super::sessions::create_session_with(state, false, &reference, false, create, None).await;
    start_outcome(response, profile, path).await
}

pub async fn prepare_suggestion(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ItemRef>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let Json(item) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let Some(provider) = find_provider(&item.provider_id) else {
        return unknown_provider();
    };
    let result = tokio::task::spawn_blocking(move || prefill_for(&host, &provider, &item.id)).await;
    match result {
        Ok(Ok(Some(Ok(prefill)))) => Json(prefill).into_response(),
        Ok(Ok(Some(Err(refusal)))) => refusal_response(refusal),
        Ok(Ok(None)) => stale_item(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
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
fn check_preset_put(
    provider: Option<Provider>,
    criterion: &str,
    config: &crate::session::config::Config,
    preset: &Preset,
) -> Result<(), axum::response::Response> {
    check_declared(provider, criterion)?;
    let invalid =
        |message: String| api_error(StatusCode::BAD_REQUEST, "validation_failed", message);
    let known_agent = crate::agents::get_agent(&preset.agent).is_some()
        || config
            .session
            .custom_agents
            .get(&preset.agent)
            .is_some_and(|command| !command.trim().is_empty());
    if !known_agent {
        return Err(invalid(format!("Unknown agent {:?}", preset.agent)));
    }
    if preset
        .prompt
        .as_deref()
        .is_some_and(|prompt| prompt.len() > crate::session::MAX_INITIAL_PROMPT_BYTES)
    {
        return Err(invalid(format!(
            "Prompt is limited to {} bytes",
            crate::session::MAX_INITIAL_PROMPT_BYTES
        )));
    }
    Ok(())
}

fn preset_json(
    provider_id: &str,
    criterion: &str,
    stored: Option<Preset>,
    defaults: &Preset,
    overridden: &[&str],
) -> serde_json::Value {
    json!({
        "provider_id": provider_id,
        "criterion": criterion,
        "customized": stored.is_some(),
        "preset": stored.as_ref().unwrap_or(defaults),
        "defaults": defaults,
        "project_overrides": overridden,
    })
}

/// The preset of every declared kind, resolved against the new-Session defaults.
pub async fn get_suggestion_presets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(host) = state.plugin_host.clone() else {
        return Json(json!({ "presets": [] })).into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        let profile = host.suggestions_profile();
        let defaults = suggestions::default_preset(
            &crate::session::config::profile_config::resolve_config_or_warn(&profile),
        );
        let overridden = suggestions::project_default_overrides(
            &profile,
            &crate::session::projects::load_merged(&profile)?,
            &defaults,
        );
        let mut stored: std::collections::HashMap<(String, String), Preset> = host
            .with_suggestions_db(suggestions::presets)?
            .into_iter()
            .map(|(provider_id, criterion, preset)| ((provider_id, criterion), preset))
            .collect();
        let presets: Vec<_> = suggestions::providers()
            .into_iter()
            .flat_map(|provider| {
                provider
                    .spec
                    .sources
                    .iter()
                    .map(|source| (provider.id.clone(), source.criterion.clone()))
                    .collect::<Vec<_>>()
            })
            .map(|key| {
                let preset = stored.remove(&key);
                preset_json(&key.0, &key.1, preset, &defaults, &overridden)
            })
            .collect();
        Ok::<_, anyhow::Error>(json!({ "presets": presets }))
    })
    .await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn put_suggestion_preset(
    State(state): State<Arc<AppState>>,
    body: Result<Json<PresetBody>, axum::extract::rejection::JsonRejection>,
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
    let provider = find_provider(&body.provider_id);
    let result = tokio::task::spawn_blocking(move || {
        // The profile config is a disk read: keep it off the executor.
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &host.suggestions_profile(),
        );
        if let Err(response) = check_preset_put(provider, &body.criterion, &config, &body.preset) {
            return Ok(Err(response));
        }
        let defaults = suggestions::default_preset(&config);
        let overridden = suggestions::project_default_overrides(
            &host.suggestions_profile(),
            &crate::session::projects::load_merged(&host.suggestions_profile())?,
            &defaults,
        );
        host.with_suggestions_db(|conn, profile| {
            suggestions::set_preset(
                conn,
                profile,
                &body.provider_id,
                &body.criterion,
                &body.preset,
            )
        })?;
        Ok::<_, anyhow::Error>(Ok(preset_json(
            &body.provider_id,
            &body.criterion,
            Some(body.preset),
            &defaults,
            &overridden,
        )))
    })
    .await;
    match result {
        Ok(Ok(Ok(body))) => Json(body).into_response(),
        Ok(Ok(Err(response))) => response,
        Ok(Err(error)) => internal(error),
        Err(error) => internal(error),
    }
}

pub async fn delete_suggestion_preset(
    State(state): State<Arc<AppState>>,
    query: Result<Query<PresetKey>, axum::extract::rejection::QueryRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Query(key) = match query {
        Ok(query) => query,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    // No provider or criterion check: a stored preset must stay resettable
    // after its plugin is disabled or drops the criterion.
    let result = tokio::task::spawn_blocking(move || {
        host.with_suggestions_db(|conn, profile| {
            suggestions::delete_preset(conn, profile, &key.provider_id, &key.criterion)
        })
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
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

/// The Projects a mapping may name, each with the `owner/repo` of its git
/// `origin` (or `null`), so a client can propose mappings. Mutates nothing.
pub async fn list_suggestion_projects(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(host) = state.plugin_host.clone() else {
        return unavailable();
    };
    let result = tokio::task::spawn_blocking(move || {
        let registered = crate::session::projects::load_merged(&host.suggestions_profile())?;
        let projects: Vec<_> = registered
            .into_iter()
            .map(|p| {
                let origin = crate::git::get_remote_slug(std::path::Path::new(&p.path));
                json!({ "name": p.name, "path": p.path, "origin": origin })
            })
            .collect();
        Ok::<_, anyhow::Error>(json!({ "projects": projects }))
    })
    .await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
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
    fn preset_put_is_bounded_and_needs_a_declared_source_and_known_agent() {
        let max = crate::session::MAX_INITIAL_PROMPT_BYTES;
        let preset = |agent: &str, len: usize| Preset {
            agent: agent.into(),
            view: suggestions::PresetView::Structured,
            worktree: true,
            sandbox: false,
            yolo: false,
            prompt: Some("x".repeat(len)),
        };
        let status = |provider: Option<Provider>, criterion: &str, preset: &Preset| {
            check_preset_put(
                provider,
                criterion,
                &crate::session::config::Config::default(),
                preset,
            )
            .err()
            .map(|r| r.status())
        };
        assert_eq!(
            status(Some(provider()), "assigned", &preset("claude", 0)),
            None
        );
        assert_eq!(
            status(Some(provider()), "assigned", &preset("claude", max)),
            None
        );
        for (provider, criterion, preset, expected) in [
            (
                Some(provider()),
                "assigned",
                preset("claude", max + 1),
                StatusCode::BAD_REQUEST,
            ),
            (
                Some(provider()),
                "undeclared",
                preset("claude", 1),
                StatusCode::BAD_REQUEST,
            ),
            (
                Some(provider()),
                "assigned",
                preset("no-such-agent", 1),
                StatusCode::BAD_REQUEST,
            ),
            (None, "assigned", preset("claude", 1), StatusCode::NOT_FOUND),
        ] {
            assert_eq!(status(provider, criterion, &preset), Some(expected));
        }
    }
}

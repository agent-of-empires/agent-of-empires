use super::AppState;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct ProfileQuery {
    profile: Option<String>,
}

fn resolve_profile(
    query: ProfileQuery,
    state: &AppState,
) -> Result<String, axum::response::Response> {
    let profile = query.profile.unwrap_or_else(|| state.profile.clone());
    if let Some(response) = super::unknown_profile_response(&profile) {
        return Err(response);
    }
    Ok(profile)
}

pub async fn get_suggestions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProfileQuery>,
) -> impl IntoResponse {
    let profile = match resolve_profile(query, &state) {
        Ok(profile) => profile,
        Err(response) => return response,
    };
    let mut snapshot = match &state.plugin_host {
        Some(host) => match host.suggestions_snapshot(&profile) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error":"internal","message":error.to_string()})),
                )
                    .into_response();
            }
        },
        None => serde_json::json!({
            "items":[],
            "providers":[],
            "auto_create":{"enabled":false,"rules":[],"last_error":null}
        }),
    };

    if let Some(items) = snapshot
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
    {
        let instances = state.instances.read().await;
        for item in items {
            let source_ref = item.get("source_ref").and_then(serde_json::Value::as_str);
            let linked = source_ref.and_then(|source| {
                super::sessions::find_by_source_ref(&instances, &profile, source)
            });
            if let Some(instance) = linked {
                item["session_id"] = serde_json::json!(instance.id);
                item["project_path"] = serde_json::json!(instance.project_path);
                item["can_start"] = serde_json::json!(false);
                item["start_blocked_reason"] =
                    serde_json::json!("This exact source is already linked to a session");
            } else {
                let project = item.get("project").and_then(serde_json::Value::as_str);
                if let Some(project) = project {
                    match super::sessions::resolve_registered_suggestion_project(&profile, project)
                        .await
                    {
                        Ok(path) => {
                            item["project_path"] = serde_json::json!(path);
                            item["can_start"] = serde_json::json!(true);
                            item["start_blocked_reason"] = serde_json::Value::Null;
                        }
                        Err(message) => {
                            item["project_path"] = serde_json::Value::Null;
                            item["can_start"] = serde_json::json!(false);
                            item["start_blocked_reason"] = serde_json::json!(message);
                        }
                    }
                } else {
                    item["project_path"] = serde_json::Value::Null;
                    item["can_start"] = serde_json::json!(false);
                    item["start_blocked_reason"] = serde_json::json!(
                        "Provider did not select a registered project or local path"
                    );
                }
                item["session_id"] = serde_json::Value::Null;
            }
            if state.read_only || state.cityhall_mode {
                item["can_start"] = serde_json::json!(false);
                item["start_blocked_reason"] = serde_json::json!("Suggestions are read-only");
            }
        }
    }
    Json(snapshot).into_response()
}

pub async fn refresh_suggestions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProfileQuery>,
) -> impl IntoResponse {
    if state.read_only || state.cityhall_mode {
        return super::read_only_response();
    }
    let profile = match resolve_profile(query, &state) {
        Ok(profile) => profile,
        Err(response) => return response,
    };
    let notified = match &state.plugin_host {
        Some(host) => host.refresh_suggestions(&profile).await,
        None => 0,
    };
    Json(serde_json::json!({"ok":true,"notified":notified})).into_response()
}
#[derive(Deserialize)]
pub struct SuggestionActionBody {
    #[serde(flatten)]
    pub suggestion: crate::plugin::suggestions::SuggestionIdentity,
    pub action: String,
    #[serde(default)]
    pub create_another: bool,
    #[serde(skip)]
    auto_create: bool,
}
pub async fn prepare_suggestion(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProfileQuery>,
    Json(identity): Json<crate::plugin::suggestions::SuggestionIdentity>,
) -> impl IntoResponse {
    if state.read_only || state.cityhall_mode {
        return super::read_only_response();
    }
    let profile = match resolve_profile(query, &state) {
        Ok(profile) => profile,
        Err(response) => return response,
    };
    let Some(host) = &state.plugin_host else {
        return super::api_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Plugin host is unavailable",
        );
    };
    match host.prepare_suggestion(&profile, &identity) {
        Ok(Some(mut value)) => {
            value["project_path"] = match value["project_path"].as_str() {
                Some(project) => {
                    super::sessions::resolve_registered_suggestion_project(&profile, project)
                        .await
                        .map(|path| serde_json::json!(path))
                        .unwrap_or(serde_json::Value::Null)
                }
                None => serde_json::Value::Null,
            };
            Json(value).into_response()
        }
        Ok(None) => super::api_error(
            axum::http::StatusCode::CONFLICT,
            "stale_suggestion",
            "Suggestion was retracted or changed; refresh before preparing it",
        ),
        Err(error) => super::api_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            error.to_string(),
        ),
    }
}

pub async fn suggestion_action(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProfileQuery>,
    Json(body): Json<SuggestionActionBody>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let profile = match resolve_profile(query, &state) {
        Ok(profile) => profile,
        Err(response) => return response,
    };
    match body.action.as_str() {
        "dismiss" | "restore" => {
            let dismissed = body.action == "dismiss";
            match state
                .plugin_host
                .as_ref()
                .map(|h| h.dismiss_suggestion(&profile, &body.suggestion, dismissed))
            {
                Some(Ok(true)) => Json(serde_json::json!({"ok":true})).into_response(),
                Some(Ok(false)) => (
                    axum::http::StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error":"stale_suggestion",
                        "message":"Suggestion was retracted or changed"
                    })),
                )
                    .into_response(),
                Some(Err(e)) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error":"internal","message":e.to_string()})),
                )
                    .into_response(),
                None => (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error":"unavailable","message":"Plugin host is unavailable"})),
                )
                    .into_response(),
            }
        }
        "start" => {
            if state.cityhall_mode {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error":"cityhall_suggestions_disabled","message":"Suggestion sessions are unavailable in CityHall mode"})),
                )
                    .into_response();
            }
            let prepared = match state
                .plugin_host
                .as_ref()
                .and_then(|h| h.prepare_suggestion(&profile, &body.suggestion).ok())
                .flatten()
            {
                Some(prepared) => prepared,
                None => {
                    return (
                        axum::http::StatusCode::CONFLICT,
                        Json(serde_json::json!({"error":"stale_suggestion","message":"Suggestion was retracted or changed; refresh before starting it"})),
                    )
                        .into_response();
                }
            };
            let Some(project) = prepared
                .get("project_path")
                .and_then(serde_json::Value::as_str)
            else {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error":"suggestion_project_required",
                        "message":"Suggestion must map to an exact registered project"
                    })),
                )
                    .into_response();
            };
            let project_path = match super::sessions::resolve_registered_suggestion_project(&profile, project).await {
                Ok(path) => path,
                Err(message) => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error":"suggestion_project_invalid",
                            "message":message
                        })),
                    )
                        .into_response();
                }
            };
            let mut config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(&profile, &project_path);
            match crate::session::projects::find_by_canonical_path_strict(&profile, &project_path) {
                Ok(Some(project)) => {
                    if let Some(enabled) = project.overrides.worktree_enabled {
                        config.worktree.enabled = enabled;
                    }
                    if let Some(enabled) = project.overrides.sandbox_enabled {
                        config.sandbox.enabled_by_default = enabled;
                    }
                }
                Ok(None) => return super::api_error(axum::http::StatusCode::BAD_REQUEST,
                    "suggestion_project_invalid", "Suggestion project is no longer registered"),
                Err(error) => return super::api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "suggestion_project_invalid", error.to_string()),
            }
            let session = &config.session;
            let tool = session
                .default_tool
                .clone()
                .unwrap_or_else(|| "claude".to_string());
            let structured = config.acp.default_new_session_view != crate::session::config::NewSessionView::Terminal
                && super::sessions::agent_is_acp_capable(&profile, &project_path, &tool, None);
            if body.auto_create {
                use aoe_plugin_api::acp::ApprovalClass;
                use crate::plugin::automation_policy::{classify_mode, ModeDecision};
                let agent_key = tool.as_str();
                let mode = config.acp.acp_defaults.get(agent_key).and_then(|defaults| defaults.mode());
                if !matches!(classify_mode(agent_key, mode.as_deref(), None),
                    ModeDecision::Class(ApprovalClass::Interactive | ApprovalClass::Guarded))
                {
                    return super::api_error(axum::http::StatusCode::FORBIDDEN,
                        "suggestion_unattended_disabled",
                        "Automatic suggestions require a known interactive or guarded approval mode");
                }
            }
            let prompt = prepared.get("prompt").and_then(serde_json::Value::as_str).unwrap_or_default();
            let title = prepared.get("title").and_then(serde_json::Value::as_str).unwrap_or_default();
            let source_ref = prepared.get("source_ref").and_then(serde_json::Value::as_str).unwrap_or_default();
            if source_ref.is_empty() || prompt.is_empty() || title.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"invalid_suggestion","message":"Suggestion is missing its source or draft content"})),
                )
                    .into_response();
            }
            let body_value = serde_json::json!({
                "title": title,
                "path": project_path.to_string_lossy(),
                "tool": tool,
                "group": "",
                "profile": profile,
                "prompt": prompt,
                "suggestion": body.suggestion,
                "source_refs": [source_ref],
                "create_another": body.create_another,
                "yolo_mode": if body.auto_create { false } else { session.yolo_mode_default },
                "sandbox": config.sandbox.enabled_by_default,
                "sandbox_image": config.sandbox.default_image,
                "scratch": false,
                "trust_hooks": false,
                "worktree_enabled": config.worktree.enabled,
                "extra_args": session.agent_extra_args.get(&tool).cloned().unwrap_or_default(),
                "extra_env": if config.sandbox.enabled_by_default { &config.sandbox.environment } else { &config.environment },
                "extra_repo_paths": [],
                "command_override": session.agent_command_override.get(&tool).cloned().unwrap_or_default(),
                "view": if structured { "structured" } else { "terminal" }
            });
            let create_body: crate::server::api::sessions::CreateSessionBody =
                match serde_json::from_value(body_value) {
                    Ok(body) => body,
                    Err(error) => {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error":"internal","message":error.to_string()})),
                        )
                            .into_response();
                    }
                };
            let response = super::sessions::create_session(
                State(state),
                Query(super::sessions::CreateSessionQuery { wait: None }),
                Ok(Json(create_body)),
            )
            .await
            .into_response();
            if !response.status().is_success() {
                return response;
            }
            let created = response.status() == axum::http::StatusCode::CREATED;
            let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error":"internal","message":error.to_string()})),
                    )
                        .into_response();
                }
            };
            let response: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(response) => response,
                Err(error) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error":"internal","message":error.to_string()})),
                    )
                        .into_response();
                }
            };
            match response.get("id").and_then(serde_json::Value::as_str) {
                Some(session_id) => Json(serde_json::json!({"session_id":session_id,"created":created})).into_response(),
                None => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error":"internal","message":"Session creation response omitted its id"})),
                )
                    .into_response(),
            }
        }
        _ => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid_action","message":"action must be dismiss, restore, or start"})),
        )
            .into_response(),
    }
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoCreateRule {
    provider_id: String,
    criterion: String,
}

#[derive(Deserialize)]
pub struct PolicyBody {
    enabled: bool,
    rules: Vec<AutoCreateRule>,
}

pub async fn update_suggestion_policy(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProfileQuery>,
    Json(body): Json<PolicyBody>,
) -> impl IntoResponse {
    if state.read_only || state.cityhall_mode {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"suggestions_disabled","message":"Suggestion policy updates are unavailable in read-only or CityHall mode"})),
        )
            .into_response();
    }
    let profile = match resolve_profile(query, &state) {
        Ok(profile) => profile,
        Err(response) => return response,
    };
    for rule in &body.rules {
        let valid_provider = crate::plugin::registry()
            .get(&rule.provider_id)
            .is_some_and(|plugin| {
                plugin.active() && plugin.manifest.runtime.is_some() && plugin.manifest.suggestions
            });
        if !valid_provider || rule.criterion.trim().is_empty() || rule.criterion.len() > 256 {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error":"validation_failed",
                    "message":"Each rule must name an active suggestion provider and a non-empty criterion of at most 256 bytes"
                })),
            )
                .into_response();
        }
    }
    let rules = serde_json::to_value(&body.rules).expect("serializing auto-create rules");
    match state.plugin_host.as_ref() {
        Some(host) => match host.update_suggestion_policy(&profile, body.enabled, &rules) {
            Ok(()) => {
                Json(serde_json::json!({"enabled":body.enabled,"rules":rules})).into_response()
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"internal","message":e.to_string()})),
            )
                .into_response(),
        },
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"unavailable","message":"Plugin host is unavailable"})),
        )
            .into_response(),
    }
}

pub(crate) fn spawn_auto_create(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => break,
                _ = interval.tick() => run_auto_create(state.clone()).await,
            }
        }
    });
}

async fn run_auto_create(state: Arc<AppState>) {
    if state.read_only || state.cityhall_mode {
        return;
    }
    let Some(host) = state.plugin_host.as_ref() else {
        return;
    };
    let profile = state.profile.clone();
    let snapshot = match host.suggestions_snapshot(&profile) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let message = error.to_string();
            let _ = host.set_suggestion_policy_error(&profile, Some(&message));
            return;
        }
    };
    let auto_create = &snapshot["auto_create"];
    if auto_create["enabled"] != true {
        return;
    }
    let rules: Vec<AutoCreateRule> = match serde_json::from_value(auto_create["rules"].clone()) {
        Ok(rules) => rules,
        Err(error) => {
            let message = error.to_string();
            let _ = host.set_suggestion_policy_error(&profile, Some(&message));
            return;
        }
    };
    let healthy_providers: std::collections::HashSet<&str> = snapshot["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|provider| provider["status"] == "ok")
        .filter_map(|provider| provider["provider_id"].as_str())
        .collect();
    let Some(items) = snapshot["items"].as_array() else {
        let _ = host.set_suggestion_policy_error(
            &profile,
            Some("Suggestion snapshot did not contain an item list"),
        );
        return;
    };
    let linked_sources: std::collections::HashSet<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|instance| instance.source_profile == profile)
        .flat_map(|instance| instance.source_refs.iter().cloned())
        .collect();
    let mut last_error = None;
    for item in items {
        if item["dismissed"] == true || item["session_id"].is_string() {
            continue;
        }
        let Some(provider_id) = item["provider_id"].as_str() else {
            continue;
        };
        let Some(criterion) = item["criterion"].as_str() else {
            continue;
        };
        let Some(source_ref) = item["source_ref"].as_str() else {
            continue;
        };
        if linked_sources.contains(source_ref)
            || !healthy_providers.contains(provider_id)
            || !rules
                .iter()
                .any(|rule| rule.provider_id == provider_id && rule.criterion == criterion)
        {
            continue;
        }
        let (Some(id), Some(revision)) = (item["id"].as_str(), item["revision"].as_i64()) else {
            continue;
        };
        let response = suggestion_action(
            State(state.clone()),
            Query(ProfileQuery {
                profile: Some(profile.clone()),
            }),
            Json(SuggestionActionBody {
                suggestion: crate::plugin::suggestions::SuggestionIdentity {
                    provider_id: provider_id.to_owned(),
                    id: id.to_owned(),
                    revision,
                },
                action: "start".to_owned(),
                create_another: false,
                auto_create: true,
            }),
        )
        .await
        .into_response();
        if !response.status().is_success() {
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024).await;
            last_error = Some(match bytes {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) => error.to_string(),
            });
        }
    }
    if let Err(error) = host.set_suggestion_policy_error(&profile, last_error.as_deref()) {
        tracing::warn!(target: "server.suggestions", error = %error, "failed to update auto-create policy status");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_request_accepts_flat_suggestion_identity() {
        let body: SuggestionActionBody = serde_json::from_value(serde_json::json!({
            "provider_id": "github",
            "id": "owner/repo/pull/42",
            "revision": 3,
            "action": "start",
            "create_another": false
        }))
        .unwrap();

        assert_eq!(body.suggestion.provider_id, "github");
        assert_eq!(body.suggestion.id, "owner/repo/pull/42");
        assert_eq!(body.suggestion.revision, 3);
        assert_eq!(body.action, "start");
        assert!(!body.create_another);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn suggestions_automation_requires_opt_in_exact_rules_and_healthy_items() {
        use crate::plugin::{host::PluginHost, host_api::HostApiState};
        let _home = crate::session::test_support::isolate_app_dir();
        crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
        let app = crate::session::get_app_dir().unwrap();
        let host = PluginHost::new(&app, "test", None).unwrap();
        let publisher = HostApiState::open(&app.join("plugin_events.db"), "test", 100).unwrap();
        let generation = publisher.begin_ui_generation("example.provider");
        let item = aoe_plugin_api::Suggestion {
            id: "42".into(),
            title: "Review".into(),
            reason: "Assigned".into(),
            source_url: "https://example.com/42".into(),
            source_ref: "https://example.com/42".into(),
            preview: "source text".into(),
            project: None,
            criterion: "assigned".into(),
            draft: aoe_plugin_api::SuggestionDraft {
                title: "Review".into(),
                prompt: "Review work".into(),
            },
        };
        publisher
            .publish_suggestions(
                "example.provider",
                generation,
                "Example",
                std::slice::from_ref(&item),
            )
            .unwrap();
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).unwrap().plugin_host = Some(host.clone());
        run_auto_create(state.clone()).await;
        assert_eq!(
            host.suggestions_snapshot("test").unwrap()["auto_create"]["enabled"],
            false
        );
        host.update_suggestion_policy(
            "test",
            true,
            &serde_json::json!([
                {"provider_id":"example.provider","criterion":"different"}
            ]),
        )
        .unwrap();
        run_auto_create(state.clone()).await;
        assert_eq!(
            host.suggestions_snapshot("test").unwrap()["auto_create"]["last_error"],
            serde_json::Value::Null
        );
        let rules = serde_json::json!([{"provider_id":"example.provider","criterion":"assigned"}]);
        host.update_suggestion_policy("test", true, &rules).unwrap();
        publisher
            .report_suggestion_error("example.provider", generation, "Example", "offline")
            .unwrap();
        run_auto_create(state.clone()).await;
        assert_eq!(
            host.suggestions_snapshot("test").unwrap()["auto_create"]["last_error"],
            serde_json::Value::Null
        );
        publisher
            .publish_suggestions("example.provider", generation, "Example", &[item])
            .unwrap();
        run_auto_create(state.clone()).await;
        let snapshot = host.suggestions_snapshot("test").unwrap();
        assert!(snapshot["auto_create"]["last_error"]
            .as_str()
            .unwrap()
            .contains("suggestion_project_required"));
        assert!(state.instances.read().await.is_empty());
        assert!(crate::server::test_support::load_instances_from_disk_for_test("test").is_empty());
    }
}

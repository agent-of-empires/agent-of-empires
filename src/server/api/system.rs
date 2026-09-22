//! Misc system endpoints: agents, settings, themes, profiles, filesystem,
//! groups, docker, system health, devices, about.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::validate_profile_name;
use super::AppState;
use super::{api_error, read_only_response};
use crate::server::auth::AuthenticatedTokenHash;
use crate::server::auth::{handler_elevated, AuthenticatedSession, LoopbackTrusted};
use crate::session::config::settings_schema::{
    clear_path, rewrite_plugin_sections, runtime_schema, strip_local_only, validate_patch,
    validate_patch_with, PatchRejection, Scope,
};

/// Foreground state reported by one browser dashboard. Kept out of normal API
/// traffic so background polling cannot suppress a phone's push notification.
#[derive(Deserialize)]
pub struct DashboardPresenceBody {
    pub active: bool,
}

/// `POST /api/presence`. Records or clears this browser's foreground presence.
/// The device-binding header is hashed into an ephemeral per-browser key rather
/// than retained; clients without it fall back to their token owner.
pub async fn post_dashboard_presence(
    State(state): State<Arc<AppState>>,
    Extension(owner): Extension<AuthenticatedTokenHash>,
    headers: HeaderMap,
    Json(body): Json<DashboardPresenceBody>,
) -> StatusCode {
    let client = headers
        .get("x-aoe-device-binding")
        .and_then(|value| value.to_str().ok())
        .map(crate::server::push::sha256_token)
        .unwrap_or(owner.0);
    state.set_web_presence(client, body.active);
    StatusCode::NO_CONTENT
}

// --- Agents ---

#[derive(Serialize)]
pub struct AgentInfo {
    pub kind: String,
    pub name: String,
    pub binary: String,
    pub host_only: bool,
    pub installed: bool,
    pub install_hint: String,
    /// Whether the agent has a one-shot mode, so it can serve the smart-rename
    /// title call. Always false for custom agents.
    pub oneshot_capable: bool,
    /// Whether the agent can run in the structured ACP UI: a built-in with an ACP
    /// adapter, or a custom agent declaring a valid `agent_acp_cmd`.
    pub acp_capable: bool,
    /// Whether the agent's ACP adapter binary resolves on this host, not just that
    /// the registry knows one exists. Gates the wizard's "Import from Claude" tab.
    pub acp_installed: bool,
    /// Whether `[acp] allowed_agents` permits this agent. Kept separate from
    /// `acp_capable`, which states an intrinsic fact that operator policy does not
    /// change, so settings surfaces can still edit a disallowed agent's defaults.
    pub acp_allowed: bool,
    /// The ACP command a built-in agent launches, after `${aoe_data_dir}`
    /// substitution; it can differ from `binary`. Omitted for custom agents, whose
    /// command values are never serialized here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_command: Option<String>,
    /// Registry args appended to `acp_command`. Empty when there are none or for
    /// custom agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acp_args: Vec<String>,
    /// Registry lifecycle state. Omitted while Active so the common wire shape is
    /// unchanged; mirrored by `AgentLifecycleInfo` in `web/src/lib/types.ts`.
    #[serde(skip_serializing_if = "crate::agents::AgentLifecycle::is_active")]
    pub lifecycle: crate::agents::AgentLifecycle,
}

/// Resolve a built-in agent's ACP command and args from its registry spec,
/// substituting `${aoe_data_dir}` so the preview matches what the supervisor
/// runs. `(None, [])` for agents without a registry entry.
fn acp_command_fields(
    spec: Option<&crate::acp::AgentSpec>,
    data_dir: Option<&std::path::Path>,
) -> (Option<String>, Vec<String>) {
    let substitute = |value: &str| match data_dir {
        Some(dir) if value.contains("${aoe_data_dir}") => {
            value.replace("${aoe_data_dir}", &dir.to_string_lossy())
        }
        _ => value.to_string(),
    };
    match spec {
        Some(spec) => {
            let command = substitute(&spec.command);
            let args = spec.args.iter().map(|arg| substitute(arg)).collect();
            (Some(command), args)
        }
        None => (None, Vec::new()),
    }
}

fn build_custom_agent_infos(
    custom_agents: &HashMap<String, String>,
    agent_acp_cmd: &HashMap<String, String>,
    agent_detect_as: &HashMap<String, String>,
    policy: &crate::acp::agent_policy::AgentPolicy,
) -> Vec<AgentInfo> {
    let mut entries: Vec<_> = custom_agents
        .iter()
        .filter(|(name, command)| {
            !name.trim().is_empty()
                && !command.trim().is_empty()
                && crate::agents::get_agent(name).is_none()
        })
        .map(|(name, _command)| AgentInfo {
            lifecycle: crate::agents::AgentLifecycle::Active,
            kind: "custom".to_string(),
            name: name.clone(),
            binary: name.clone(),
            host_only: false,
            installed: true,
            install_hint: "Configured custom agent".to_string(),
            oneshot_capable: false,
            acp_capable: agent_acp_cmd
                .get(name)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(name, cmd).is_ok())
                || crate::acp::inherited_acp_base(name, agent_detect_as).is_some(),
            acp_allowed: policy.allows(name),
            // A custom agent's acp_command is never serialized (it can hold
            // hostnames or secrets), so its install state is not probed.
            acp_installed: false,
            acp_command: None,
            acp_args: Vec::new(),
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

pub async fn list_agents(State(state): State<Arc<AppState>>) -> Json<Vec<AgentInfo>> {
    let profile = state.profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        let custom_agents = config.session.custom_agents;
        let agent_acp_cmd = config.session.agent_acp_cmd;
        let agent_detect_as = config.session.agent_detect_as;
        let tools = crate::tmux::AvailableTools::detect();
        let available = tools.available_list();
        let acp_registry = crate::acp::AgentRegistry::with_defaults();
        // Global config, not `config` above: the allowlist is an operator
        // control and a profile override must not widen it (#3241).
        let policy = crate::acp::agent_policy::AgentPolicy::load();
        let data_dir = crate::session::get_app_dir().ok();
        let mut agents = crate::agents::AGENTS
            .iter()
            .map(|a| {
                let (acp_command, acp_args) =
                    acp_command_fields(acp_registry.get(a.name), data_dir.as_deref());
                AgentInfo {
                    kind: "builtin".to_string(),
                    name: a.name.to_string(),
                    binary: a.binary.to_string(),
                    host_only: a.host_only,
                    installed: available.iter().any(|s| s == a.name),
                    install_hint: a.install_hint.to_string(),
                    oneshot_capable: a.oneshot_flag.is_some(),
                    lifecycle: a.lifecycle,
                    acp_capable: acp_registry.get(a.name).is_some(),
                    acp_installed: acp_command
                        .as_deref()
                        .is_some_and(crate::cli::acp::command_present),
                    acp_allowed: policy.allows(a.name),
                    acp_command,
                    acp_args,
                }
            })
            .collect::<Vec<_>>();
        agents.extend(build_custom_agent_infos(
            &custom_agents,
            &agent_acp_cmd,
            &agent_detect_as,
            &policy,
        ));
        agents
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!("list_agents task failed: {e}");
        Vec::new()
    });
    Json(result)
}

// --- Settings ---

#[derive(Deserialize)]
pub struct SettingsQuery {
    pub profile: Option<String>,
}

pub async fn get_settings(
    axum::extract::Query(query): axum::extract::Query<SettingsQuery>,
) -> impl IntoResponse {
    let config_result = if let Some(ref profile_name) = query.profile {
        crate::session::resolve_config(profile_name)
    } else {
        crate::session::Config::load()
    };

    match config_result {
        Ok(config) => match serde_json::to_value(&config) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => {
                tracing::error!(target: "http.api.system", "Settings serialization failed: {}", e);
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "serialize_failed",
                    "Failed to serialize settings",
                )
            }
        },
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings load failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                "Failed to load settings",
            )
        }
    }
}

/// Map a schema [`PatchRejection`] onto the dashboard's HTTP shape.
/// `elevation_required` mirrors the path-shape gate's 403 so the web client's
/// interceptor fires the passphrase prompt unchanged.
fn reject_response(rej: PatchRejection) -> axum::response::Response {
    let status = StatusCode::from_u16(rej.status_code()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(serde_json::json!({"error": rej.error_code(), "message": rej.message()})),
    )
        .into_response()
}

/// Persist a global patch and apply side effects for the sections it changes.
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall exposes only the theme control, which writes through
    // `PATCH /api/theme`; the general settings PATCH stays fully closed (#7).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    if state.read_only {
        return read_only_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Strip host-execution surfaces (`local_only`) first, so a bundled or
    // echoed-back patch keeps its safe leaves and drops the rest (#1692).
    strip_local_only(&mut body);
    // Validate every remaining leaf against the runtime schema (core plus
    // active-plugin `plugin:<id>` sections). The auth middleware already
    // elevation-gates this route, so anything reaching here counts as elevated.
    if let Err(rej) = validate_patch_with(&runtime_schema(), &body, Scope::Global, true) {
        return reject_response(rej);
    }
    // Record which plugins the patch touches BEFORE the rewrite folds them into
    // `plugins.<id>.settings.*`, so `plugin.settings.changed` can be emitted
    // after a successful write (#2897).
    let plugin_changes: Vec<(String, Vec<String>)> = body
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(section, value)| {
                    let id = crate::session::config::settings_schema::section_plugin_id(section)?;
                    let keys: Vec<String> = value
                        .as_object()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    Some((id.to_string(), keys))
                })
                .collect()
        })
        .unwrap_or_default();
    rewrite_plugin_sections(&mut body);

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_config(|config| -> anyhow::Result<_> {
            let mut current = serde_json::to_value(&*config)?;
            crate::session::config::settings_schema::merge_json(&mut current, &body);
            // The target editor sends the complete map, including removals.
            if let Some(targets) = body.pointer("/logging/targets") {
                current["logging"]["targets"] = if targets.is_null() {
                    serde_json::json!({})
                } else {
                    targets.clone()
                };
            }
            let updated: crate::session::Config = serde_json::from_value(current)?;
            let logging_changed = config.logging.default_level != updated.logging.default_level
                || config.logging.targets != updated.logging.targets;
            *config = updated;
            Ok((config.clone(), logging_changed))
        })
        .and_then(|inner| inner)
    })
    .await;

    match result {
        Ok(Ok((config, logging_changed))) => {
            // No-op and restart-only edits preserve temporary runtime filters.
            if logging_changed {
                if let Ok(app_dir) = crate::session::get_app_dir() {
                    crate::logging::apply_persisted_config(
                        &config.logging.default_level,
                        &config.logging.targets,
                        &app_dir,
                    );
                }
            }
            // Notify each touched plugin's worker after the durable write (#2897).
            if !plugin_changes.is_empty() {
                if let Some(host) = &state.plugin_host {
                    host.emit_settings_changed(&plugin_changes).await;
                }
            }
            match serde_json::to_value(&config) {
                Ok(val) => (StatusCode::OK, Json(val)).into_response(),
                Err(e) => {
                    tracing::error!(target: "http.api.system", "Settings serialization failed: {}", e);
                    api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "serialize_failed",
                        "Failed to serialize settings",
                    )
                }
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Settings update failed: {}", e);
            api_error(
                StatusCode::BAD_REQUEST,
                "update_failed",
                "Failed to update settings",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings update panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// `GET /api/cityhall/bundle` returns this install's CityHall config bundle as
/// TOML. Refused in CityHall client mode: `cityhall_gate` only guards
/// mutations, so this read needs its own block.
pub async fn get_cityhall_bundle(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let built = tokio::task::spawn_blocking(|| {
        crate::session::cityhall_bundle::export().and_then(|b| b.to_toml())
    })
    .await;
    match built {
        Ok(Ok(toml)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/toml")],
            toml,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.system", "CityHall bundle export failed: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "export_failed",
                e.to_string(),
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "CityHall bundle export panicked: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// `GET /api/settings/schema` returns the flat list of settings field
/// descriptors the dashboard renders generic field components from, so a new
/// config field appears on the web automatically (#1692). Pure metadata, so
/// normal authentication is enough.
pub async fn get_settings_schema(
) -> Json<Vec<crate::session::config::settings_schema::FieldDescriptor>> {
    Json(runtime_schema())
}

/// `GET /api/settings/resolved` returns every setting's effective value plus
/// its provenance chain, so the dashboard can show where a value comes from.
pub async fn get_settings_resolved(
) -> Json<Vec<crate::session::config::settings_schema::ResolvedSetting>> {
    Json(
        tokio::task::spawn_blocking(crate::session::config::settings_schema::resolve_all)
            .await
            .unwrap_or_default(),
    )
}

/// Body of `PATCH /api/theme`. Either field may be omitted to leave it unchanged.
#[derive(serde::Deserialize)]
pub struct ThemePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color_mode: Option<crate::session::config::ColorMode>,
}

/// `PATCH /api/theme` sets the global theme name and/or color mode and returns
/// the freshly resolved theme so the caller can repaint. Theme is global, never
/// per profile. Deliberately non-elevated, like the web-tour flag: a cosmetic
/// change must not trip the passphrase wall. `read_only` still blocks it.
pub async fn update_theme(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ThemePatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(mut patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // CityHall hides the color-mode control, so only the name is writable (#7).
    if state.cityhall_mode {
        patch.color_mode = None;
    }
    // Reject an unknown name so a typo cannot silently repaint to `default`.
    // Empty is allowed and clears back to the default builtin.
    if let Some(name) = &patch.name {
        if !name.is_empty()
            && !crate::tui::styles::available_themes()
                .iter()
                .any(|t| t == name)
        {
            return api_error(
                StatusCode::BAD_REQUEST,
                "unknown_theme",
                format!("Unknown theme '{name}'"),
            );
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        // `update_config` re-loads via `Config::load()`, so a corrupt
        // config.toml errors out instead of being replaced with defaults.
        let theme_name = crate::session::update_config(|config| {
            if let Some(name) = patch.name {
                config.theme.name = name;
            }
            if let Some(mode) = patch.color_mode {
                config.theme.color_mode = mode;
            }
            config.effective_theme_name()
        })?;
        Ok::<_, anyhow::Error>(crate::tui::styles::resolve_theme(&theme_name))
    })
    .await;

    match result {
        Ok(Ok(resolved)) => match serde_json::to_value(&resolved) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => {
                tracing::error!(target: "http.api.system", "theme serialization failed: {}", e);
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "serialize_failed",
                    "Failed to serialize theme",
                )
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "theme update failed: {}", e);
            api_error(
                StatusCode::BAD_REQUEST,
                "update_failed",
                "Failed to update theme",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "theme update panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Marks the web dashboard's first-run tour as seen.
///
/// Single-purpose write so the cosmetic flag never widens the security-sensitive
/// `PATCH /api/settings` surface, and deliberately exempt from the elevation
/// wall; `read_only` still blocks it. Persisted to `state.toml`, so a corrupt
/// `config.toml` cannot block it.
pub async fn mark_web_tour_seen(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        crate::session::update_app_state(|state| {
            state.has_seen_web_tour = true;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"has_seen_web_tour": true})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking web tour seen failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tour state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking web tour seen panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(Serialize)]
pub struct TipDto {
    pub id: String,
    pub title: String,
    pub body: String,
    pub seen: bool,
}

#[derive(Serialize)]
pub struct TipsResponse {
    /// Mirror of `session.show_tips`; the dashboard hides the badge and panel
    /// when false. Written through `POST /api/tips/show`, not here.
    pub enabled: bool,
    /// Web-eligible tips in catalog order, each flagged as seen. The frontend
    /// derives the badge count from the unseen ones.
    pub tips: Vec<TipDto>,
}

/// Returns the web-surface tips and whether tips are enabled, composed from the
/// `crate::tips` catalog plus `app_state.tips_seen` and `session.show_tips`.
pub async fn get_tips(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        let signals = crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
            system_health_tip_earned: config.app_state.system_health_tip_earned,
            used_system_health: config.app_state.used_system_health,
        };
        let seen = &config.app_state.tips_seen;
        let tips = crate::tips::eligible(crate::tips::TipSurface::Web, &signals)
            .into_iter()
            .map(|tip| TipDto {
                id: tip.id.to_string(),
                title: tip.title.to_string(),
                body: tip.body.to_string(),
                seen: seen.iter().any(|s| s == tip.id),
            })
            .collect();
        Ok::<_, anyhow::Error>(TipsResponse {
            enabled: config.session.show_tips,
            tips,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        // Best-effort: an unreadable config yields an empty, disabled payload
        // so the dashboard shows no badge rather than erroring.
        _ => (
            StatusCode::OK,
            Json(TipsResponse {
                enabled: false,
                tips: Vec::new(),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct MarkTipSeenBody {
    pub id: String,
}

/// Marks one tip seen in the shared `app_state.tips_seen`, so mark-seen-on-view
/// sticks across devices. Rejects an id outside the catalog so junk cannot
/// accumulate. Exempt from elevation like [`mark_web_tour_seen`].
pub async fn mark_tip_seen(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MarkTipSeenBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(MarkTipSeenBody { id }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if !crate::tips::id_in_catalog(&id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "unknown_tip",
            format!("Unknown tip id '{id}'"),
        );
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            if !state.tips_seen.iter().any(|s| s == &id) {
                state.tips_seen.push(id);
            }
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking tip seen failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tip state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking tip seen panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(Deserialize)]
pub struct SetShowTipsBody {
    pub enabled: bool,
}

/// Sets `session.show_tips`, the "Show tips on startup" checkbox. A cosmetic
/// preference, so it is a dedicated write exempt from the elevation wall that
/// guards `PATCH /api/settings`; `read_only` still blocks it.
pub async fn set_show_tips(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SetShowTipsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(SetShowTipsBody { enabled }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_config(|config| {
            config.session.show_tips = enabled;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"show_tips": enabled})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Setting show_tips failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tips state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Setting show_tips panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DismissUpdateBody {
    pub version: String,
}

/// Records the version whose update banner the user dismissed, in the shared
/// `app_state.dismissed_update_version`, so the dismissal sticks across devices
/// and matches the TUI. Exempt from elevation like [`mark_web_tour_seen`].
pub async fn dismiss_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DismissUpdateBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let version = body.version;
    let persisted = version.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            state.dismissed_update_version = Some(persisted);
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"dismissed_version": version})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Dismissing update failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist dismissal",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Dismissing update panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Returns the dashboard's server-side UI-state blob (`app_state.web_ui_state`):
/// a flat map of frontend localStorage keys to opaque string values. Exposes only
/// UI preferences, so the normal token wall is enough.
pub async fn get_web_ui_state(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        Ok::<_, anyhow::Error>(config.app_state.web_ui_state)
    })
    .await;
    match result {
        Ok(Ok(map)) => (StatusCode::OK, Json(serde_json::json!(map))).into_response(),
        // Best-effort: an unreadable config yields an empty blob, so the
        // dashboard falls back to its localStorage cache.
        _ => (StatusCode::OK, Json(serde_json::json!({}))).into_response(),
    }
}

/// Merges a partial update into `app_state.web_ui_state`: a string value sets a
/// key, `null` deletes it. Non-string, non-null values are ignored since
/// localStorage values are always strings. Exempt from elevation like
/// [`mark_web_tour_seen`].
pub async fn patch_web_ui_state(
    State(state): State<Arc<AppState>>,
    body: Result<
        Json<serde_json::Map<String, serde_json::Value>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Reject anything that is not a string (set) or null (delete), so a client
    // regression surfaces instead of silently dropping part of the sync.
    let invalid: Vec<&String> = patch
        .iter()
        .filter(|(_, v)| !v.is_string() && !v.is_null())
        .map(|(k, _)| k)
        .collect();
    if !invalid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_value_type",
                "message": "web UI state values must be string or null",
                "keys": invalid,
            })),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            for (key, value) in patch {
                match value {
                    serde_json::Value::Null => {
                        state.web_ui_state.remove(&key);
                    }
                    serde_json::Value::String(s) => {
                        state.web_ui_state.insert(key, s);
                    }
                    // Already rejected above; kept exhaustive.
                    _ => {}
                }
            }
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Persisting web UI state failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist UI state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Persisting web UI state panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Records that the user acknowledged glob `volume_ignores` snapshot expansion
/// (#2045), so the wizard's confirm modal is shown once. Exempt from elevation
/// like [`mark_web_tour_seen`].
pub async fn mark_volume_ignores_globs_acknowledged(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        crate::session::update_app_state(|state| {
            state.has_acknowledged_volume_ignores_globs = true;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"has_acknowledged_volume_ignores_globs": true})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking volume_ignores globs acknowledged failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist acknowledgment",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking volume_ignores globs acknowledged panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

// --- Themes ---

pub async fn list_themes() -> Json<Vec<String>> {
    Json(
        crate::tui::styles::available_themes()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    )
}

/// Upper bound on the `:name` path segment for `/api/themes/:name`. Past the
/// cap we resolve Empire without logging the body, to keep tracing sane under
/// fuzzing.
const MAX_THEME_NAME_LEN: usize = 128;

/// `GET /api/themes/:name` returns the resolved theme projection for the named
/// theme. Unknown names resolve to the `default` builtin with
/// `source: "fallback"`, mirroring `load_theme`. Runs in `spawn_blocking`: the
/// resolver does sync file I/O.
pub async fn get_resolved_theme(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<crate::tui::styles::ResolvedTheme> {
    if name.len() > MAX_THEME_NAME_LEN {
        tracing::warn!(
            len = name.len(),
            "GET /api/themes/{{name}} rejected: name exceeds {} bytes",
            MAX_THEME_NAME_LEN,
        );
        return Json(crate::tui::styles::resolve_theme("zinc"));
    }
    tracing::debug!(theme = %name, "GET /api/themes/{{name}}");
    let resolved = tokio::task::spawn_blocking(move || crate::tui::styles::resolve_theme(&name))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "theme resolve task panicked, falling back to default");
            crate::tui::styles::resolve_theme("zinc")
        });
    Json(resolved)
}

/// `GET /api/theme/current` returns the resolved theme to paint. Theme is a
/// global preference, not profile-merged, so every surface paints the same one.
pub async fn get_current_theme(
    State(state): State<Arc<AppState>>,
) -> Json<crate::tui::styles::ResolvedTheme> {
    let profile = state.profile.clone();
    tracing::debug!(profile = %profile, "GET /api/theme/current");
    let resolved = tokio::task::spawn_blocking(move || {
        let name = crate::session::config::resolve_theme_name();
        crate::tui::styles::resolve_theme(&name)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "current theme resolve task panicked, falling back to default");
        crate::tui::styles::resolve_theme("zinc")
    });
    Json(resolved)
}

// --- Wizard support ---

#[derive(Serialize)]
pub struct ProfileInfo {
    pub name: String,
    pub is_default: bool,
    /// Optional short description, shown as helper text in the wizard profile
    /// picker. Omitted when the profile has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub async fn list_profiles(State(state): State<Arc<AppState>>) -> Json<Vec<ProfileInfo>> {
    // Profile enumeration and description lookups all hit disk; keep them off
    // the async runtime so a slow filesystem cannot stall Tokio workers.
    let active_profile = state.profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Resolve the active profile before enumerating: on a genuine first run
        // resolution bootstraps `main` and creates its directory, which must
        // happen before `list_profiles()` or the new profile would be missing.
        let active: String = if active_profile.is_empty() {
            crate::session::config::resolve_default_profile()
        } else {
            active_profile
        };
        // Picker order (`default` last); `active` came from the enumeration.
        let profiles = crate::session::list_profiles_for_display().unwrap_or_default();
        profiles
            .into_iter()
            .map(|name| {
                let is_default = name == active;
                let description = crate::session::load_profile_config(&name)
                    .ok()
                    .and_then(|c| c.description);
                ProfileInfo {
                    name,
                    is_default,
                    description,
                }
            })
            .collect::<Vec<ProfileInfo>>()
    })
    .await
    .unwrap_or_default();
    Json(result)
}

#[derive(Deserialize)]
pub struct BrowseQuery {
    pub path: String,
    pub limit: Option<usize>,
    pub filter: Option<String>,
    /// Include dotfile-prefixed directories, mirroring the TUI picker's Ctrl+H
    /// toggle. Omitted means false.
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
}

#[derive(Serialize)]
struct BrowseResponse {
    entries: Vec<DirEntry>,
    has_more: bool,
}

pub async fn filesystem_home(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    match dirs::home_dir() {
        Some(home) => (
            StatusCode::OK,
            Json(serde_json::json!({"path": home.to_string_lossy()})),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Could not determine home directory"})),
        )
            .into_response(),
    }
}

/// Standard system folders directly under $HOME. On macOS several are
/// TCC-protected, so opening them prompts. None is ever a git repo, so the
/// browser skips its `.git` probe for them.
const HOME_SYSTEM_DIRS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Public",
    "Library",
];

/// The TCC prompt is macOS-only, so gate the skip there.
fn skip_git_probe(parent: &std::path::Path, name: &str, home: Option<&std::path::Path>) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    match home {
        Some(home) => parent == home && HOME_SYSTEM_DIRS.contains(&name),
        None => false,
    }
}

pub async fn browse_filesystem(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<BrowseQuery>,
) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        let limit = query.limit.unwrap_or(100);
        let filter = query.filter.map(|f| f.trim().to_lowercase());
        let path = std::path::Path::new(&query.path);
        let canonical = path.canonicalize().map_err(|_| "Path does not exist")?;

        if !canonical.is_dir() {
            return Err("Path is not a directory");
        }

        let home = dirs::home_dir();
        // Security: restrict browsing to the user's home directory
        if let Some(home) = &home {
            if !canonical.starts_with(home) {
                return Err("Path is outside the home directory");
            }
        }

        let mut entries: Vec<DirEntry> = Vec::new();
        let read_dir = std::fs::read_dir(&canonical).map_err(|_| "Cannot read directory")?;

        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !query.show_hidden && name.starts_with('.') {
                continue;
            }
            let entry_path = entry.path();
            let is_dir = entry_path.is_dir();
            if !is_dir {
                continue;
            }
            if let Some(filter) = filter.as_deref() {
                if !filter.is_empty() && !name.to_lowercase().contains(filter) {
                    continue;
                }
            }
            // Probing `.git` opens the directory, which prompts under macOS
            // TCC. None of the standard $HOME folders is ever a repo.
            let is_git_repo = if skip_git_probe(&canonical, &name, home.as_deref()) {
                false
            } else {
                entry_path.join(".git").exists()
            };
            entries.push(DirEntry {
                name,
                path: entry_path.to_string_lossy().to_string(),
                is_dir,
                is_git_repo,
            });
        }
        // Cached: `sort_by_cached_key` calls the keyfn O(n) times rather than
        // O(n log n), so the lowercase String is allocated once per entry.
        entries.sort_by_cached_key(|e| e.name.to_lowercase());
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(BrowseResponse { entries, has_more })
    })
    .await;

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Ok(Err(msg)) => api_error(StatusCode::BAD_REQUEST, "browse_failed", msg),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Serialize)]
pub struct GroupInfo {
    pub path: String,
    pub session_count: usize,
}

pub async fn list_groups(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let instances = state.instances.read().await;
    let mut group_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for inst in instances.iter() {
        if !inst.group_path.is_empty() {
            *group_counts.entry(inst.group_path.clone()).or_default() += 1;
        }
    }
    let groups: Vec<GroupInfo> = group_counts
        .into_iter()
        .map(|(path, session_count)| GroupInfo {
            path,
            session_count,
        })
        .collect();
    Json(groups)
}

/// One agent row on the health readout. Every figure is optional for the same
/// reason as on `AgentMetric`: a sandboxed agent's numbers come from the
/// container runtime, which may have no sample to give.
#[derive(Serialize)]
pub struct SystemHealthAgent {
    pub id: String,
    pub title: String,
    pub cpu_fraction: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub procs: Option<usize>,
    pub sandboxed: bool,
}

/// Host headroom plus per-agent usage, as the system-health strip reads it.
/// `status` is the server's own worst-of classification, so both dashboards
/// band a reading identically rather than each re-deriving the thresholds.
#[derive(Serialize)]
pub struct SystemHealth {
    pub status: &'static str,
    pub cpu_fraction: Option<f64>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub load_average: Option<[f64; 3]>,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub agent_count: usize,
    pub proc_count: usize,
    pub agents: Vec<SystemHealthAgent>,
}

pub async fn system_health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    use crate::process::metrics::pressure_band;

    // The per-agent rows are the population CityHall hides: the sampler's
    // `eligible_instance` selects the non-structured sessions that
    // `list_sessions` filters out and `sessions/search` refuses, so serving
    // this would hand a locked-down client their ids and titles. See #7.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }

    let instances = state.instances.read().await.clone();
    // Sampling walks the host process table, shells out to tmux, and may read
    // container stats, so it runs off the async workers: a tmux stall would
    // otherwise hold one for as long as its timeout. The sampler's lock is
    // taken inside that work, not around it, so two concurrent polls still
    // cannot interleave their CPU deltas and report nonsense to both.
    let sampler_state = Arc::clone(&state);
    let snapshot = tokio::task::spawn_blocking(move || {
        let mut sampler = sampler_state.metrics_sampler.blocking_lock();
        sampler.sample(&instances)
    })
    .await
    .unwrap_or_default();

    Json(SystemHealth {
        status: pressure_band(&snapshot.memory).as_str(),
        cpu_fraction: snapshot.system.cpu_fraction,
        memory_used_bytes: snapshot.memory.used_bytes(),
        memory_total_bytes: snapshot.memory.total_bytes,
        load_average: snapshot.system.load_average,
        swap_used_bytes: snapshot.system.swap_used_bytes,
        swap_total_bytes: snapshot.system.swap_total_bytes,
        agent_count: snapshot.counts.agents,
        proc_count: snapshot.counts.procs,
        agents: snapshot
            .agents
            .into_iter()
            .map(|a| SystemHealthAgent {
                id: a.id,
                title: a.title,
                cpu_fraction: a.cpu_fraction,
                memory_bytes: a.rss_bytes,
                procs: a.procs,
                sandboxed: a.sandboxed,
            })
            .collect(),
    })
    .into_response()
}

#[derive(Serialize)]
pub struct DockerStatus {
    pub available: bool,
    pub runtime: Option<String>,
}

pub async fn docker_status() -> Json<DockerStatus> {
    let result = tokio::task::spawn_blocking(|| {
        let runtime = crate::containers::get_container_runtime();
        let available = runtime.is_available() && runtime.is_daemon_running();
        let runtime_name = if available {
            let config = crate::session::Config::load_or_warn();
            Some(
                serde_json::to_value(config.sandbox.container_runtime)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "docker".to_string()),
            )
        } else {
            None
        };
        DockerStatus {
            available,
            runtime: runtime_name,
        }
    })
    .await
    .unwrap_or(DockerStatus {
        available: false,
        runtime: None,
    });
    Json(result)
}

/// Read-only runtime view of the `aoe serve` daemon's sleep-inhibit reconciler,
/// derived from the poll loop's snapshot plus the live backend latch.
#[derive(Serialize)]
pub struct SleepInhibitStatus {
    /// The `session.prevent_sleep_when_active` toggle as the reconciler last
    /// read it: the raw config toggle only.
    pub prevent_sleep_enabled: bool,
    /// Whether the daemon holds an OS sleep assertion as of the last reconcile,
    /// so it can trail the death of the backing child by up to the poll interval.
    /// Requires both a retained slot and an available backend.
    pub currently_held: bool,
    /// Whether a real OS backend is still believed able to hold the assertion.
    /// Optimistic: `true` only means no failure has latched yet, since the
    /// backend is never actively probed.
    pub backend_available: bool,
}

/// Fold the reconciler snapshot and the live backend-availability read into the
/// reported status. `currently_held` gates the retained slot on the backend being
/// available, so a doomed slot kept to suppress respawns never reports held.
fn derive_sleep_inhibit_status(
    prevent_sleep_enabled: bool,
    slot_present: bool,
    backend_available: bool,
) -> SleepInhibitStatus {
    SleepInhibitStatus {
        prevent_sleep_enabled,
        currently_held: slot_present && backend_available,
        backend_available,
    }
}

#[derive(Serialize)]
pub struct ServerAbout {
    pub version: String,
    pub auth_required: bool,
    pub passphrase_enabled: bool,
    /// Resolved `--auth` mode: `"token"`, `"passphrase"`, or `"none"`. Derived
    /// from the token and login managers because the CLI mode itself is not
    /// retained in `AppState`.
    pub auth_mode: &'static str,
    pub read_only: bool,
    pub behind_tunnel: bool,
    /// CityHall client mode (`AOE_CITYHALL_MODE`), which drives the dashboard's
    /// locked-down end-user client. See #7.
    pub cityhall_mode: bool,
    pub profile: String,
    /// Resolved `acp.show_tool_durations`, driving the per-tool elapsed-time
    /// label in the web UI.
    pub acp_show_tool_durations: bool,
    /// Resolved `acp.replay_events`: per-session retention cap on the acp event
    /// log, 0 for unlimited. The web client mirrors it on its in-memory activity
    /// buffer instead of clipping at a hard-coded constant (#1111).
    pub acp_replay_events: u32,
    /// Resolved `acp.compaction_reminder`, gating the structured view's
    /// compaction reminder. Off by default.
    pub acp_compaction_reminder: bool,
    /// Resolved `acp.compaction_reminder_percent`: the context-window
    /// percentage at which the reminder appears.
    pub acp_compaction_reminder_percent: u8,
    /// `"debug"` when built with `debug_assertions`, else `"release"`. The web
    /// UI renders a DEV badge from it so concurrent debug (8081) and release
    /// (8080) instances are distinguishable, PWA installs included.
    pub build_flavor: &'static str,
    /// Content-hashed entry bundle name of the embedded dashboard build. The
    /// client compares it against its own entry script tag and offers a reload
    /// when they differ, since installed PWAs have no refresh affordance.
    pub web_build_id: Option<&'static str>,
    /// Read-only runtime state of the daemon's sleep-inhibit reconciler.
    pub sleep_inhibit: SleepInhibitStatus,
}

pub async fn get_about(State(state): State<Arc<AppState>>) -> Json<ServerAbout> {
    let auth_required = !state.token_manager.is_no_auth().await;
    let passphrase_enabled = state.login_manager.is_enabled();
    let auth_mode =
        crate::server::resolve_auth_mode(&state.token_manager, &state.login_manager).await;
    let acp_cfg =
        crate::session::config::profile_config::resolve_config_or_warn(&state.profile).acp;
    let acp_show_tool_durations = acp_cfg.show_tool_durations;
    let acp_replay_events = acp_cfg.replay_events;
    let acp_compaction_reminder = acp_cfg.compaction_reminder;
    let acp_compaction_reminder_percent = acp_cfg.compaction_reminder_percent;
    let snapshot = state
        .sleep_inhibit_snapshot
        .load(std::sync::atomic::Ordering::Relaxed);
    let sleep_inhibit = derive_sleep_inhibit_status(
        snapshot & crate::server::SLEEP_INHIBIT_SNAPSHOT_ENABLED != 0,
        snapshot & crate::server::SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT != 0,
        crate::process::sleep_inhibit_backend_available(),
    );
    Json(ServerAbout {
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_required,
        passphrase_enabled,
        auth_mode,
        read_only: state.read_only,
        behind_tunnel: state.behind_tunnel,
        cityhall_mode: state.cityhall_mode,
        profile: state.profile.clone(),
        acp_show_tool_durations,
        acp_replay_events,
        acp_compaction_reminder,
        acp_compaction_reminder_percent,
        build_flavor: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        web_build_id: crate::server::web_build_id(),
        sleep_inhibit,
    })
}

// --- Update status ---

/// Web-facing snapshot of `update::check_for_update`. `update_check_mode`
/// mirrors `updates.update_check_mode` so the frontend can hide its banner or
/// skip nagging during a background install without fetching settings.
#[derive(Serialize)]
pub struct UpdateStatusResponse {
    pub update_check_mode: crate::session::config::UpdateCheckMode,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    /// Set when the GitHub check failed (e.g. rate-limited, offline).
    /// Frontend keeps polling on its normal cadence; the banner stays
    /// hidden until a successful poll. The error is exposed so the
    /// settings UI can surface a one-liner if useful later.
    pub error: Option<String>,
    /// The version the user has already dismissed the update banner for,
    /// from the shared `app_state.dismissed_update_version`. Server-side so a
    /// dismissal sticks across devices and matches the TUI (#release-notes
    /// should only be acknowledged once, not per browser).
    pub dismissed_version: Option<String>,
}

pub async fn get_update_status(State(state): State<Arc<AppState>>) -> Json<UpdateStatusResponse> {
    let cfg = crate::session::config::profile_config::resolve_config_or_warn(&state.profile);
    let current = env!("CARGO_PKG_VERSION").to_string();
    let mode = cfg.updates.update_check_mode;

    if !mode.is_enabled() {
        return Json(UpdateStatusResponse {
            update_check_mode: mode,
            current_version: current,
            latest_version: None,
            update_available: false,
            release_url: None,
            error: None,
            dismissed_version: cfg.app_state.dismissed_update_version.clone(),
        });
    }

    match crate::update::check_for_update(&current, false).await {
        Ok(info) => {
            let release_url = if info.latest_version.is_empty() {
                None
            } else {
                Some(crate::update::release_page_url(&info.latest_version))
            };
            Json(UpdateStatusResponse {
                update_check_mode: mode,
                current_version: info.current_version,
                latest_version: if info.latest_version.is_empty() {
                    None
                } else {
                    Some(info.latest_version)
                },
                update_available: info.available,
                release_url,
                error: None,
                dismissed_version: cfg.app_state.dismissed_update_version.clone(),
            })
        }
        Err(e) => Json(UpdateStatusResponse {
            update_check_mode: mode,
            current_version: current,
            latest_version: None,
            update_available: false,
            release_url: None,
            error: Some(e.to_string()),
            dismissed_version: cfg.app_state.dismissed_update_version.clone(),
        }),
    }
}

// --- Profile management ---

#[derive(Deserialize)]
pub struct CreateProfileBody {
    pub name: String,
}

pub async fn create_profile(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CreateProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // Profiles are hidden entirely in CityHall (no picker, no CRUD UI).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&body.name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    let name_for_create = body.name.clone();
    match tokio::task::spawn_blocking(move || crate::session::create_profile(&name_for_create))
        .await
    {
        Ok(Ok(())) => {
            crate::server::add_profile_disk_watch(&state, &body.name).await;
            (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "create_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub async fn delete_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // Profiles are hidden entirely in CityHall (no picker, no CRUD UI).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    if name == state.profile {
        return api_error(
            StatusCode::BAD_REQUEST,
            "active_profile",
            "Cannot delete the active profile",
        );
    }
    let name_for_delete = name.clone();
    match tokio::task::spawn_blocking(move || crate::session::delete_profile(&name_for_delete))
        .await
    {
        Ok(Ok(())) => {
            crate::server::remove_profile_disk_watch(&state, &name).await;
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "delete_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RenameProfileBody {
    pub new_name: String,
}

pub async fn rename_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: Result<Json<RenameProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // Profiles are hidden entirely in CityHall (no picker, no CRUD UI).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    if let Err(e) = validate_profile_name(&body.new_name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    let old = name;
    let new = body.new_name;
    let old_for_rewire = old.clone();
    let new_for_rewire = new.clone();
    match tokio::task::spawn_blocking(move || crate::session::rename_profile(&old, &new)).await {
        Ok(Ok(())) => {
            crate::server::rename_profile_disk_watch(&state, &old_for_rewire, &new_for_rewire)
                .await;
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "rename_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct DefaultProfileBody {
    pub name: String,
}

pub async fn default_profile(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DefaultProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // Profiles are hidden entirely in CityHall (no picker, no CRUD UI).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&body.name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    let name = body.name;
    match tokio::task::spawn_blocking(move || crate::session::set_default_profile(&name)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "update_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub async fn get_profile_settings(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    let result = tokio::task::spawn_blocking(move || {
        let profile = crate::session::load_profile_config(&name)?;
        let global = crate::session::Config::load_or_warn();
        let mut val = serde_json::to_value(&profile)?;
        // `logging` lives on the global Config with no profile override
        // surface, so splice it in; otherwise the settings dropdowns reset on
        // every page load even after a successful PATCH.
        if let Some(obj) = val.as_object_mut() {
            obj.insert(
                "logging".to_string(),
                serde_json::to_value(&global.logging)?,
            );
            // Plugin settings are global-only at Tier 0, so splice them in or
            // the dashboard reverts to manifest defaults on every profile-view
            // load (#2094).
            obj.insert(
                "plugins".to_string(),
                serde_json::to_value(&global.plugins)?,
            );
        }
        Ok::<_, anyhow::Error>(val)
    })
    .await;
    match result {
        Ok(Ok(val)) => (StatusCode::OK, Json(val)).into_response(),
        Ok(Err(e)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "load_failed",
            e.to_string(),
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// Leaf paths CityHall mode may write through the profile-settings PATCH: only
/// the curated trash cluster the Sessions tab exposes, so an endpoint that must
/// stay open cannot double as an arbitrary profile-override writer (#7).
const CITYHALL_PROFILE_LEAVES: &[&str] = &[
    "session.delete_to_trash",
    "session.confirm_delete",
    "session.trash_retention_days",
];

/// Walk a sparse settings patch and return the first dotted leaf path not in
/// [`CITYHALL_PROFILE_LEAVES`], or `None` when every leaf is permitted.
fn first_non_cityhall_profile_leaf(patch: &serde_json::Value) -> Option<String> {
    fn walk(prefix: &str, v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    if let Some(bad) = walk(&path, child) {
                        return Some(bad);
                    }
                }
                None
            }
            _ if CITYHALL_PROFILE_LEAVES.contains(&prefix) => None,
            _ => Some(prefix.to_string()),
        }
    }
    walk("", patch)
}

pub async fn update_profile_settings(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    // Reject any leaf outside the CityHall allowlist, so the endpoint kept open
    // for the Sessions trash toggles cannot write arbitrary overrides (#7).
    if state.cityhall_mode {
        if let Some(bad) = first_non_cityhall_profile_leaf(&body) {
            return api_error(
                StatusCode::FORBIDDEN,
                "cityhall_mode",
                format!("Field '{bad}' is not writable in CityHall mode"),
            );
        }
    }
    // Elevation up front: login disabled means always elevated, a
    // loopback-trusted caller is elevated per the #1168 carve-out (#2610).
    let elevated = handler_elevated(&state, session.as_deref(), loopback.is_some()).await;

    // Validate every leaf against the schema (#1692). An
    // elevation_required 403 mirrors the path-shape gate's payload so
    // web/src/lib/fetchInterceptor.ts fires the passphrase prompt (#1510).
    // `description` is profile-only and rejected on the global endpoint.
    if let Err(rej) = validate_patch(&body, Scope::Profile, elevated) {
        return reject_response(rej);
    }

    // Validate scope before stripping, including global-only local fields.
    strip_local_only(&mut body);

    let result = tokio::task::spawn_blocking(move || {
        let config = crate::session::load_profile_config(&name).unwrap_or_default();
        let mut current = serde_json::to_value(&config)?;
        // Apply each validated leaf onto the sparse override object: null
        // clears it, anything else sets it. Sections are created lazily so a
        // single-field patch never wipes its siblings.
        if let Some(update_obj) = body.as_object() {
            for (key, value) in update_obj {
                match value {
                    serde_json::Value::Object(fields) => {
                        for (field, fval) in fields {
                            if fval.is_null() {
                                clear_path(&mut current, key, field);
                            } else if let Some(root) = current.as_object_mut() {
                                let section = root
                                    .entry(key.clone())
                                    .or_insert_with(|| serde_json::json!({}));
                                if let Some(sec) = section.as_object_mut() {
                                    sec.insert(field.clone(), fval.clone());
                                }
                            }
                        }
                    }
                    serde_json::Value::Null => {
                        if let Some(root) = current.as_object_mut() {
                            root.remove(key);
                        }
                    }
                    other => {
                        if let Some(root) = current.as_object_mut() {
                            root.insert(key.clone(), other.clone());
                        }
                    }
                }
            }
        }
        let config: crate::session::ProfileConfig = serde_json::from_value(current)?;
        crate::session::save_profile_config(&name, &config)?;
        Ok::<_, anyhow::Error>(config)
    })
    .await;

    match result {
        Ok(Ok(config)) => match serde_json::to_value(&config) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "serialize_failed",
                e.to_string(),
            ),
        },
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "update_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

// --- Sounds ---

pub async fn list_sounds() -> Json<Vec<String>> {
    Json(crate::sound::list_available_sounds())
}

/// Serve a sound file by name so the acp's browser-side approval player can
/// fetch it same-origin. The name is validated against
/// `list_available_sounds()`, so this cannot read arbitrary disk paths.
pub async fn serve_sound_file(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    // `list_available_sounds` does a sync `read_dir`, so validation stays on
    // the blocking pool; the file read uses `tokio::fs::read`.
    let lookup_name = name.clone();
    let validated = tokio::task::spawn_blocking(move || {
        if !crate::sound::list_available_sounds().contains(&lookup_name) {
            return None;
        }
        crate::sound::get_sounds_dir().map(|dir| dir.join(&lookup_name))
    })
    .await;

    let path = match validated {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let content_type = match std::path::Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("ogg") => "audio/ogg",
        _ => "audio/wav",
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::collections::HashMap;

    #[test]
    fn derive_sleep_inhibit_status_gates_held_on_backend() {
        // The last two rows are unreachable from the writer but pin the pure
        // gate, proving `currently_held` excludes prevent_sleep_enabled.
        // (prevent_sleep_enabled, slot_present, backend_available) -> currently_held
        let cases = [
            // supported host actively holding the assertion
            ((true, true, true), true),
            // toggle on but every session idle past grace: slot released
            ((true, false, true), false),
            // backend latched unavailable or no-op platform: the slot is kept
            // to suppress respawns, yet no real assertion is held
            ((true, true, false), false),
            // gate guard: enabled must not force held when the backend is down
            ((false, true, false), false),
            // gate guard: held tracks slot AND backend, never prevent_sleep_enabled
            ((false, true, true), true),
        ];
        for ((enabled, slot, avail), held) in cases {
            let s = derive_sleep_inhibit_status(enabled, slot, avail);
            assert_eq!(
                s.currently_held, held,
                "enabled={enabled} slot={slot} avail={avail}"
            );
            assert_eq!(s.prevent_sleep_enabled, enabled);
            assert_eq!(s.backend_available, avail);
        }
    }

    #[test]
    fn cityhall_profile_leaf_allows_only_the_curated_trash_cluster() {
        // Every curated leaf, on its own and bundled, is permitted.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({
                "session": {
                    "delete_to_trash": true,
                    "confirm_delete": false,
                    "trash_retention_days": 30
                }
            })),
            None
        );
        // An empty patch is a no-op, not a violation.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({"session": {}})),
            None
        );
        // Any leaf outside the allowlist is reported by its dotted path.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({
                "session": {"delete_to_trash": true, "yolo_mode": true}
            })),
            Some("session.yolo_mode".to_string())
        );
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({"theme": {"name": "x"}})),
            Some("theme.name".to_string())
        );
    }

    #[test]
    fn skip_git_probe_avoids_protected_home_folders() {
        let home = std::path::Path::new("/Users/alice");

        // The TCC prompt is macOS-only, so other platforms probe normally.
        let macos = cfg!(target_os = "macos");

        // Protected folders directly under $HOME: skipped on macOS.
        for name in ["Downloads", "Desktop", "Music", "Pictures", "Documents"] {
            assert_eq!(
                skip_git_probe(home, name, Some(home)),
                macos,
                "unexpected probe decision for ~/{name}"
            );
        }

        // A real project directly under $HOME still gets probed, so its git
        // badge is preserved.
        assert!(!skip_git_probe(home, "myproject", Some(home)));

        // A system-looking name that is not directly under $HOME is probed.
        let sub = std::path::Path::new("/Users/alice/code");
        assert!(!skip_git_probe(sub, "Downloads", Some(home)));

        // No home resolved: never skip (falls back to normal probing).
        assert!(!skip_git_probe(home, "Downloads", None));
    }

    fn custom_agents(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(name, command)| ((*name).to_string(), (*command).to_string()))
            .collect()
    }

    /// The default policy, for tests that predate the allowlist.
    fn unrestricted() -> crate::acp::agent_policy::AgentPolicy {
        crate::acp::agent_policy::AgentPolicy::for_test(false, &[])
    }

    /// #3241: policy is a separate axis from capability. A disallowed agent
    /// still reports `acp_capable: true` so the settings surfaces can edit its
    /// per-agent defaults; only `acp_allowed` goes false.
    #[test]
    fn acp_allowed_is_independent_of_acp_capable() {
        let custom = custom_agents(&[("oc-sp", "ocp run sp"), ("blocked", "ssh host claude")]);
        let acp = custom_agents(&[
            ("oc-sp", "ocp run sp acp"),
            ("blocked", "ocp run blocked acp"),
        ]);
        let policy = crate::acp::agent_policy::AgentPolicy::for_test(true, &["oc-sp"]);
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &policy);

        let oc_sp = entries.iter().find(|e| e.name == "oc-sp").unwrap();
        assert!(oc_sp.acp_capable && oc_sp.acp_allowed);

        let blocked = entries.iter().find(|e| e.name == "blocked").unwrap();
        assert!(
            blocked.acp_capable,
            "capability is intrinsic and policy must not erase it"
        );
        assert!(!blocked.acp_allowed, "policy denies this agent");

        // Unrestricted leaves both true, so the default path is unchanged.
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &unrestricted());
        assert!(entries.iter().all(|e| e.acp_capable && e.acp_allowed));
    }

    #[test]
    fn custom_agent_entries_use_safe_placeholders() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-claude", "ssh -t prod.example claude")]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );

        assert_eq!(entries.len(), 1);
        let agent = &entries[0];
        assert_eq!(agent.kind, "custom");
        assert_eq!(agent.name, "remote-claude");
        assert_eq!(agent.binary, "remote-claude");
        assert!(!agent.host_only);
        assert!(agent.installed);
        assert_eq!(agent.install_hint, "Configured custom agent");
        // No agent_acp_cmd configured, so it is tmux-only.
        assert!(!agent.acp_capable);
    }

    #[test]
    fn serialized_custom_agent_response_contains_no_command_or_detect_as_data() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-agent", "ssh -t prod.example claude")]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );
        let value = serde_json::to_value(&entries).unwrap();

        assert_eq!(value[0]["kind"], "custom");
        assert_eq!(value[0]["name"], "remote-agent");
        assert_eq!(value[0]["binary"], "remote-agent");
        assert_eq!(value[0]["installed"], true);
        assert_eq!(value[0]["host_only"], false);
        assert_eq!(value[0]["install_hint"], "Configured custom agent");

        let serialized = value.to_string();
        assert!(!serialized.contains("ssh -t prod.example claude"));
        assert!(!serialized.contains("prod.example"));
        assert!(!serialized.contains("agent_detect_as"));
    }

    #[test]
    fn agent_info_lifecycle_wire_shape() {
        // The /api/agents contract: lifecycle omitted for Active agents, full
        // metadata for deprecated ones. Mirrored by web/src/lib/types.ts.
        let mk = |name: &str| {
            let def = crate::agents::get_agent(name).unwrap();
            AgentInfo {
                kind: "builtin".to_string(),
                name: def.name.to_string(),
                binary: def.binary.to_string(),
                host_only: def.host_only,
                installed: true,
                install_hint: def.install_hint.to_string(),
                oneshot_capable: def.oneshot_flag.is_some(),
                acp_capable: false,
                acp_installed: false,
                acp_allowed: true,
                acp_command: None,
                acp_args: Vec::new(),
                lifecycle: def.lifecycle,
            }
        };
        let claude = serde_json::to_value(mk("claude")).unwrap();
        assert!(claude.get("lifecycle").is_none(), "{claude}");
        let gemini = serde_json::to_value(mk("gemini")).unwrap();
        assert_eq!(gemini["lifecycle"]["state"], "deprecated", "{gemini}");
        assert_eq!(gemini["lifecycle"]["since"], "2026-06-18");
        assert_eq!(gemini["lifecycle"]["replacement"], "antigravity");
    }

    #[test]
    fn custom_agent_entries_filter_empty_values_and_builtin_collisions() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[
                ("", "codex"),
                ("empty-command", ""),
                ("   ", "codex"),
                ("whitespace-command", "   "),
                ("claude", "ssh -t prod.example claude"),
                ("remote-codex", "ssh -t prod.example codex"),
            ]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["remote-codex"]);
        assert!(entries.iter().all(|entry| entry.kind == "custom"));
    }

    #[test]
    fn custom_agent_entries_are_sorted_by_name() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[
                ("zeta", "zeta-cmd"),
                ("alpha", "alpha-cmd"),
                ("middle", "middle-cmd"),
            ]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zeta"]);
    }

    #[test]
    fn custom_agent_acp_capable_tracks_agent_acp_cmd() {
        let custom = custom_agents(&[("oc-sp", "ocp run sp"), ("plain", "ssh host claude")]);
        let acp = custom_agents(&[
            ("oc-sp", "ocp run sp acp"),
            // An entry whose command is malformed must not flip capability on.
            ("broken", "ocp run \"unterminated"),
        ]);
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &unrestricted());

        let oc_sp = entries.iter().find(|e| e.name == "oc-sp").unwrap();
        assert!(oc_sp.acp_capable, "agent with a valid acp cmd is capable");
        let plain = entries.iter().find(|e| e.name == "plain").unwrap();
        assert!(!plain.acp_capable, "agent with no acp cmd is tmux-only");
    }

    #[test]
    fn custom_agent_acp_capable_via_detect_as_inheritance() {
        // A wrapper inheriting a registry-backed base (claude) is capable
        // through that adapter; one inheriting cursor stays tmux-only.
        let custom = custom_agents(&[
            ("lenovo-claude", "CLAUDE_CONFIG_DIR=/work claude"),
            ("my-cursor", "agent"),
        ]);
        let detect_as = custom_agents(&[("lenovo-claude", "claude"), ("my-cursor", "cursor")]);
        let entries =
            build_custom_agent_infos(&custom, &HashMap::new(), &detect_as, &unrestricted());

        let lenovo = entries.iter().find(|e| e.name == "lenovo-claude").unwrap();
        assert!(
            lenovo.acp_capable,
            "wrapper inheriting claude is acp-capable via the base adapter"
        );
        let cursor = entries.iter().find(|e| e.name == "my-cursor").unwrap();
        assert!(
            !cursor.acp_capable,
            "wrapper inheriting a terminal-only base stays tmux-only"
        );
    }

    #[test]
    fn acp_command_fields_resolve_registry_command_and_args() {
        let registry = crate::acp::AgentRegistry::with_defaults();

        let (cmd, args) = acp_command_fields(registry.get("opencode"), None);
        assert_eq!(cmd.as_deref(), Some("opencode"));
        assert_eq!(args, vec!["acp".to_string()]);

        let (cmd, args) = acp_command_fields(registry.get("gemini"), None);
        assert_eq!(cmd.as_deref(), Some("gemini"));
        assert_eq!(args, vec!["--acp".to_string()]);

        let (cmd, args) = acp_command_fields(registry.get("claude"), None);
        assert_eq!(cmd.as_deref(), Some("claude-agent-acp"));
        assert!(args.is_empty());

        let (cmd, args) = acp_command_fields(None, None);
        assert_eq!(cmd, None);
        assert!(args.is_empty());
    }

    #[test]
    fn acp_command_fields_substitute_data_dir() {
        let dir = std::path::Path::new("/tmp/aoe-data");
        let spec = crate::acp::AgentSpec {
            command: "${aoe_data_dir}/bin/custom-acp".into(),
            args: vec![],
            description: "custom".into(),
            env_allowlist: None,
        };
        let (cmd, _) = acp_command_fields(Some(&spec), Some(dir));
        let cmd = cmd.expect("spec has a command");
        assert!(
            !cmd.contains("${aoe_data_dir}"),
            "placeholder must be substituted"
        );
        assert!(cmd.starts_with("/tmp/aoe-data/"));

        // The bundled agent resolves through the adapter install, so its
        // command is the bare binary token, not a data-dir path (#3553).
        let registry = crate::acp::AgentRegistry::with_defaults();
        let (cmd, _) = acp_command_fields(registry.get("aoe-agent"), Some(dir));
        assert_eq!(cmd.as_deref(), Some("aoe-agent"));
    }

    #[test]
    fn custom_agent_entries_omit_acp_command_fields() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("oc-sp", "ocp run sp")]),
            &custom_agents(&[("oc-sp", "ocp run sp acp")]),
            &HashMap::new(),
            &unrestricted(),
        );
        let value = serde_json::to_value(&entries).unwrap();
        assert!(value[0].get("acp_command").is_none());
        assert!(value[0].get("acp_args").is_none());
        // The custom acp command must not leak via the new fields.
        assert!(!value.to_string().contains("ocp run sp"));
    }

    /// A name outside `list_available_sounds()` is refused, traversal included,
    /// and a 404 that still streamed a body would be worse than the wrong
    /// status.
    #[tokio::test]
    async fn serve_sound_file_rejects_names_outside_the_sounds_dir() {
        for name in ["does-not-exist-xyz.wav", "../../../etc/passwd"] {
            let resp = serve_sound_file(axum::extract::Path(name.to_string()))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{name}");
            let body = to_bytes(resp.into_body(), 1024).await.unwrap();
            assert!(body.is_empty(), "{name}: unexpected body bytes: {body:?}");
        }
    }
}

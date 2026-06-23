//! Misc system endpoints: agents, settings, themes, profiles, filesystem,
//! groups, docker status, devices, about.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::validate_profile_name;
use super::AppState;
use crate::server::auth::AuthenticatedSession;
use crate::session::settings_schema::{
    clear_path, strip_local_only, validate_patch, PatchRejection, Scope,
};

// --- Agents ---

#[derive(Serialize)]
pub struct AgentInfo {
    pub kind: String,
    pub name: String,
    pub binary: String,
    pub host_only: bool,
    pub installed: bool,
    pub install_hint: String,
    /// True when this agent has a one-shot mode (a `oneshot_flag`), so it can
    /// be used for the smart-rename title call. The settings smart-rename agent
    /// picker filters on this together with `installed`. Always false for
    /// custom agents (no built-in one-shot contract).
    pub oneshot_capable: bool,
    /// True when this agent can run in the structured acp UI: a
    /// built-in with an ACP adapter, or a custom agent that declares a
    /// valid `agent_acp_cmd`. The web wizard reads this to decide
    /// whether a session created for the agent runs in acp or tmux.
    pub acp_capable: bool,
    /// True when the agent's ACP adapter binary (`acp_command`) is actually
    /// resolvable on this host, not just registered. Distinct from
    /// `installed` (the agent's own CLI binary) and `acp_capable` (registry
    /// knows an adapter exists). The wizard's "Import from Claude" tab gates
    /// on this so it never shows when claude-agent-acp is missing. See #2276.
    pub acp_installed: bool,
    /// The ACP command a built-in agent launches in acp, e.g.
    /// `claude-agent-acp` for claude or `opencode` for opencode. This is
    /// the registry command (post `${aoe_data_dir}` substitution), which
    /// can differ from `binary`; the wizard previews it so the user sees
    /// the real launch command before starting. Omitted for custom
    /// agents, whose command values are never serialized here (see the
    /// custom-agent serialization tests below).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_command: Option<String>,
    /// The registry args appended to `acp_command` (e.g. `["acp"]`
    /// for opencode, `["--acp"]` for gemini). Empty when there are none
    /// or for custom agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acp_args: Vec<String>,
}

/// Resolve the acp launch command + args for a built-in agent from
/// its registry spec, substituting `${aoe_data_dir}` so the preview
/// matches what `supervisor::spawn_inner` actually runs. Returns
/// `(None, [])` for agents without a registry entry.
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
) -> Vec<AgentInfo> {
    let mut entries: Vec<_> = custom_agents
        .iter()
        .filter(|(name, command)| {
            !name.trim().is_empty()
                && !command.trim().is_empty()
                && crate::agents::get_agent(name).is_none()
        })
        .map(|(name, _command)| AgentInfo {
            kind: "custom".to_string(),
            name: name.clone(),
            binary: name.clone(),
            host_only: false,
            installed: true,
            install_hint: "Configured custom agent".to_string(),
            oneshot_capable: false,
            acp_capable: agent_acp_cmd
                .get(name)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(name, cmd).is_ok()),
            // Custom agents' acp_command is never serialized here (it can hold
            // hostnames or secrets), so we don't probe its install state; the
            // import tab is claude-only regardless.
            acp_installed: false,
            // Custom agents' command values are deliberately never
            // serialized here; they can hold hostnames or secrets.
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
        let config = crate::session::profile_config::resolve_config_or_warn(&profile);
        let custom_agents = config.session.custom_agents;
        let agent_acp_cmd = config.session.agent_acp_cmd;
        let tools = crate::tmux::AvailableTools::detect();
        let available = tools.available_list();
        let acp_registry = crate::acp::AgentRegistry::with_defaults();
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
                    acp_capable: acp_registry.get(a.name).is_some(),
                    acp_installed: acp_command
                        .as_deref()
                        .is_some_and(crate::cli::acp::command_present),
                    acp_command,
                    acp_args,
                }
            })
            .collect::<Vec<_>>();
        agents.extend(build_custom_agent_infos(&custom_agents, &agent_acp_cmd));
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
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "serialize_failed", "message": "Failed to serialize settings"})),
                )
                    .into_response()
            }
        },
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings load failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "load_failed", "message": "Failed to load settings"})),
            )
                .into_response()
        }
    }
}

/// Map a schema [`PatchRejection`] to the HTTP response shape the dashboard
/// expects. `elevation_required` mirrors the path-shape gate's 403 so the web
/// client's interceptor fires the passphrase prompt unchanged.
fn reject_response(rej: PatchRejection) -> axum::response::Response {
    let status = StatusCode::from_u16(rej.status_code()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(serde_json::json!({"error": rej.error_code(), "message": rej.message()})),
    )
        .into_response()
}

pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Strip host-execution surfaces (`local_only`: node_path, agent
    // argv/command, status-hook commands) before anything else, so a bundled
    // or echoed-back patch keeps its safe leaves and silently drops the
    // local-only ones (#1692). They can never reach disk from the web.
    strip_local_only(&mut body);
    // Validate every remaining leaf against the schema (single source of
    // truth): unknown section/field -> 400, bad value -> 400. `PATCH
    // /api/settings` is already elevation-gated by the auth middleware, so any
    // field reaching here is treated as elevated.
    if let Err(rej) = validate_patch(&body, Scope::Global, true) {
        return reject_response(rej);
    }

    let result = tokio::task::spawn_blocking(move || {
        let config = crate::session::Config::load_or_warn();
        let mut current = serde_json::to_value(&config)?;
        crate::session::settings_schema::merge_json(&mut current, &body);
        let config: crate::session::Config = serde_json::from_value(current)?;
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(config)
    })
    .await;

    match result {
        Ok(Ok(config)) => {
            // Settings touched [logging]? Apply the new filter live to
            // the daemon + persist runtime_filter so acp runners pick
            // it up via the notify watcher.
            if let Ok(app_dir) = crate::session::get_app_dir() {
                crate::logging::apply_persisted_config(
                    &config.logging.default_level,
                    &config.logging.targets,
                    &app_dir,
                );
            }
            match serde_json::to_value(&config) {
                Ok(val) => (StatusCode::OK, Json(val)).into_response(),
                Err(e) => {
                    tracing::error!(target: "http.api.system", "Settings serialization failed: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "serialize_failed", "message": "Failed to serialize settings"})),
                    )
                        .into_response()
                }
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Settings update failed: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "update_failed", "message": "Failed to update settings"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings update panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// `GET /api/settings/schema` returns the flat list of settings field
/// descriptors (the single source of truth, see #1692). The web dashboard
/// renders a generic field component from this list instead of hand-written
/// per-field JSX, so a new config field appears on the web automatically. No
/// secrets: descriptors are pure metadata (labels, widgets, validation, write
/// policy), so this needs no elevation, only normal authentication.
pub async fn get_settings_schema() -> Json<Vec<crate::session::settings_schema::FieldDescriptor>> {
    Json(crate::session::settings_schema::schema())
}

/// Body of `PATCH /api/theme`. Either field may be omitted to leave it
/// unchanged.
#[derive(serde::Deserialize)]
pub struct ThemePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color_mode: Option<crate::session::config::ColorMode>,
}

/// `PATCH /api/theme` sets the global theme name and/or color mode and returns
/// the freshly resolved theme so the caller can repaint.
///
/// The theme is a global preference (see `config::resolve_theme_name`): it
/// lives in the global config, never a profile, so one theme paints every
/// surface. This is a dedicated, non-elevated write (like the web-tour flag)
/// rather than routing through `PATCH /api/settings`: a cosmetic theme change
/// must not trip the passphrase wall that guards the general settings surface
/// (`requires_elevation` keeps profile-settings preference writes off that wall
/// for the same reason). `read_only` still blocks it.
pub async fn update_theme(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ThemePatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Reject an unknown theme name so a typo can't repaint to the `default`
    // fallback. Empty is allowed (clears back to the default builtin).
    if let Some(name) = &patch.name {
        if !name.is_empty()
            && !crate::tui::styles::available_themes()
                .iter()
                .any(|t| t == name)
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "unknown_theme",
                    "message": format!("Unknown theme '{name}'"),
                })),
            )
                .into_response();
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        // `Config::load()` (not `load_or_warn`) so a corrupt config.toml is not
        // silently replaced with defaults, wiping every other setting, just to
        // change a theme. Mirrors `mark_web_tour_seen`. A parse error surfaces
        // as a 400 below.
        let mut config = crate::session::Config::load()?;
        if let Some(name) = patch.name {
            config.theme.name = name;
        }
        if let Some(mode) = patch.color_mode {
            config.theme.color_mode = mode;
        }
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(crate::tui::styles::resolve_theme(
            &config.effective_theme_name(),
        ))
    })
    .await;

    match result {
        Ok(Ok(resolved)) => match serde_json::to_value(&resolved) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => {
                tracing::error!(target: "http.api.system", "theme serialization failed: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "serialize_failed", "message": "Failed to serialize theme"})),
                )
                    .into_response()
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "theme update failed: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "update_failed", "message": "Failed to update theme"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "theme update panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// Marks the web dashboard's first-run tour as seen for this server.
///
/// Single-purpose write so the cosmetic flag never widens the
/// `PATCH /api/settings` surface (which carries security-sensitive
/// sections like `sandbox`/`worktree` and the `app_state`
/// hooks-acknowledgement field). Deliberately exempt from the
/// elevation/passphrase wall: it flips one cosmetic bool, grants no
/// capability, and `read_only` still blocks it. Uses `Config::load()`
/// (not `load_or_warn`) so a corrupt config is not silently replaced
/// with defaults just to persist this flag.
pub async fn mark_web_tour_seen(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        let mut config = crate::session::Config::load()?;
        config.app_state.has_seen_web_tour = true;
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist tour state"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking web tour seen panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
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
    /// Mirror of `session.show_tips`. The dashboard hides the badge and panel
    /// when this is false. This payload is a read projection; the toggle is
    /// written through the dedicated `POST /api/tips/show` ([`set_show_tips`]),
    /// and the same preference is also editable from the settings schema.
    pub enabled: bool,
    /// Web-eligible tips in catalog order, each flagged with whether it has been
    /// seen. The frontend derives the badge count from the unseen ones and can
    /// still show seen tips in a collapsed section.
    pub tips: Vec<TipDto>,
}

/// Returns the web-surface tips and whether tips are enabled, composed from the
/// shared `crate::tips` catalog plus `app_state.tips_seen` and
/// `session.show_tips`. A read projection, so it stays a plain GET behind the
/// token wall like [`get_web_ui_state`]; the TUI-only tips never appear here.
pub async fn get_tips(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        let signals = crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
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
        // Best-effort: an unreadable config yields an empty, disabled payload so
        // the dashboard simply shows no badge rather than erroring.
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

/// Marks one tip seen by appending its id to the shared `app_state.tips_seen`,
/// so the dashboard's mark-seen-on-view sticks across devices and matches the
/// TUI. Single-purpose write mirroring [`mark_web_tour_seen`]: exempt from the
/// elevation wall, still blocked by `read_only`, and uses `Config::load()` so a
/// corrupt config is not silently replaced. Rejects an id that is not in the
/// catalog so junk can't accumulate in the persisted seen list.
pub async fn mark_tip_seen(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MarkTipSeenBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(MarkTipSeenBody { id }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if !crate::tips::id_in_catalog(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "unknown_tip", "message": format!("Unknown tip id '{id}'")})),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(move || {
        let mut config = crate::session::Config::load()?;
        if !config.app_state.tips_seen.iter().any(|s| s == &id) {
            config.app_state.tips_seen.push(id);
        }
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking tip seen failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist tip state"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking tip seen panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SetShowTipsBody {
    pub enabled: bool,
}

/// Sets `session.show_tips`, the "Show tips on startup" checkbox in the tip-of-
/// the-day modal. A dedicated single-purpose write rather than `PATCH
/// /api/settings`, which the auth middleware elevation-gates: this is a cosmetic
/// preference and must not trip the passphrase wall on a remote server. Mirrors
/// [`mark_web_tour_seen`]: exempt from elevation, still blocked by `read_only`,
/// and uses `Config::load()` so a corrupt config is not silently replaced. The
/// same preference is also editable from the settings Interaction tab.
pub async fn set_show_tips(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SetShowTipsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(SetShowTipsBody { enabled }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let result = tokio::task::spawn_blocking(move || {
        let mut config = crate::session::Config::load()?;
        config.session.show_tips = enabled;
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist tips state"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Setting show_tips panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DismissUpdateBody {
    pub version: String,
}

/// Records that the user dismissed the update banner for a specific version,
/// persisting to the shared `app_state.dismissed_update_version` so the
/// dismissal sticks across devices (and matches the TUI's snooze) rather than
/// living in per-browser localStorage. Single-purpose write mirroring
/// [`mark_web_tour_seen`]: exempt from the elevation wall, still blocked by
/// `read_only`, and uses `Config::load()` so a corrupt config is not silently
/// replaced with defaults.
pub async fn dismiss_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DismissUpdateBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let version = body.version;
    let persisted = version.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut config = crate::session::Config::load()?;
        config.app_state.dismissed_update_version = Some(persisted);
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist dismissal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Dismissing update panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// Return the web dashboard's server-side UI-state blob (`app_state.web_ui_state`):
/// a flat map of the frontend's localStorage keys to their opaque string values.
/// Single-tenant, so this is the one user's synced prefs. GET is unauthenticated
/// beyond the normal token wall (it grants no capability and exposes only UI
/// preferences).
pub async fn get_web_ui_state(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        Ok::<_, anyhow::Error>(config.app_state.web_ui_state)
    })
    .await;
    match result {
        Ok(Ok(map)) => (StatusCode::OK, Json(serde_json::json!(map))).into_response(),
        // Best-effort: an unreadable config yields an empty blob rather than an
        // error, so the dashboard just falls back to its localStorage cache.
        _ => (StatusCode::OK, Json(serde_json::json!({}))).into_response(),
    }
}

/// Merge a partial update into `app_state.web_ui_state`. The body is a flat JSON
/// object keyed by the frontend's localStorage keys: a string value sets the
/// key, `null` deletes it. Mirrors [`mark_web_tour_seen`]'s exemptions (off the
/// elevation wall, still blocked by `read_only`, `Config::load()` to avoid
/// clobbering a corrupt config). Non-string, non-null values are ignored since
/// localStorage values are always strings.
pub async fn patch_web_ui_state(
    State(state): State<Arc<AppState>>,
    body: Result<
        Json<serde_json::Map<String, serde_json::Value>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Values must be string (set) or null (delete); reject anything else
    // explicitly so a client regression surfaces instead of silently dropping
    // part of the sync.
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
        let mut config = crate::session::Config::load()?;
        for (key, value) in patch {
            match value {
                serde_json::Value::Null => {
                    config.app_state.web_ui_state.remove(&key);
                }
                serde_json::Value::String(s) => {
                    config.app_state.web_ui_state.insert(key, s);
                }
                // Already rejected above; keep exhaustive for safety.
                _ => {}
            }
        }
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Persisting web UI state failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist UI state"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Persisting web UI state panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// Records that the user has acknowledged glob `volume_ignores` snapshot
/// expansion (#2045), so the new-session wizard's confirm modal is shown once
/// and never again. Single-purpose write mirroring [`mark_web_tour_seen`]: it
/// flips one bool, grants no capability, stays exempt from the elevation wall,
/// and `read_only` still blocks it. `Config::load()` (not `load_or_warn`) keeps
/// a corrupt config from being silently overwritten.
pub async fn mark_volume_ignores_globs_acknowledged(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        let mut config = crate::session::Config::load()?;
        config.app_state.has_acknowledged_volume_ignores_globs = true;
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(())
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "save_failed", "message": "Failed to persist acknowledgment"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking volume_ignores globs acknowledged panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
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

/// Upper bound on the `:name` path segment for `/api/themes/:name`.
/// Builtin names are <= 20 chars and custom theme filenames are
/// inherently capped by the host filesystem; 128 is far past any
/// real theme name. Past the cap we resolve Empire without logging
/// the body to keep tracing output sane under fuzzing.
const MAX_THEME_NAME_LEN: usize = 128;

/// `GET /api/themes/:name` returns the resolved theme projection (web
/// CSS vars, terminal CSS vars, syntax highlighter selection,
/// appearance) for the named theme. Unknown names resolve to the
/// `default` builtin with `source: "fallback"`, mirroring
/// `load_theme`'s behaviour.
///
/// Wrapped in `spawn_blocking`: the resolver does sync file I/O
/// (`discover_custom_themes` directory scan + TOML parse) which must
/// not run on a tokio worker thread.
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
/// global preference, not profile-merged, so this reads the global config
/// (see `config::resolve_theme_name`); every surface paints the same theme
/// regardless of the active session profile. Sync work runs in
/// `spawn_blocking`.
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
    /// Optional short description, surfaced as helper text in the wizard
    /// profile picker. `None` (and therefore omitted from JSON) when the
    /// profile has no description configured. See #949.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub async fn list_profiles(State(state): State<Arc<AppState>>) -> Json<Vec<ProfileInfo>> {
    // Profile enumeration plus per-profile description lookups all hit disk;
    // do that off the async runtime so a slow filesystem (network home, fuse,
    // etc.) cannot stall Tokio workers for every API client. See CodeRabbit
    // feedback on #1274.
    let active_profile = state.profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Resolve the active profile *before* enumerating. A server launched
        // without --profile carries an empty profile; resolution then picks
        // the first profile, bootstrapping `main` on a genuine first run.
        // That bootstrap creates the profile directory as a side effect, so
        // it must run before `list_profiles()` or the freshly bootstrapped
        // profile would be absent from the returned list.
        let active: String = if active_profile.is_empty() {
            crate::session::config::resolve_default_profile()
        } else {
            active_profile
        };
        let profiles = crate::session::list_profiles().unwrap_or_default();
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

pub async fn filesystem_home() -> impl IntoResponse {
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

pub async fn browse_filesystem(
    axum::extract::Query(query): axum::extract::Query<BrowseQuery>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let limit = query.limit.unwrap_or(100);
        let filter = query.filter.map(|f| f.trim().to_lowercase());
        let path = std::path::Path::new(&query.path);
        let canonical = path.canonicalize().map_err(|_| "Path does not exist")?;

        if !canonical.is_dir() {
            return Err("Path is not a directory");
        }

        // Security: restrict browsing to the user's home directory
        if let Some(home) = dirs::home_dir() {
            if !canonical.starts_with(&home) {
                return Err("Path is outside the home directory");
            }
        }

        let mut entries: Vec<DirEntry> = Vec::new();
        let read_dir = std::fs::read_dir(&canonical).map_err(|_| "Cannot read directory")?;

        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
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
            let is_git_repo = entry_path.join(".git").exists();
            entries.push(DirEntry {
                name,
                path: entry_path.to_string_lossy().to_string(),
                is_dir,
                is_git_repo,
            });
        }
        // Cached: avoids re-allocating the lowercase String on every comparison
        // (sort_by_key calls the keyfn O(n log n) times, sort_by_cached_key calls it O(n)).
        entries.sort_by_cached_key(|e| e.name.to_lowercase());
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(BrowseResponse { entries, has_more })
    })
    .await;

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Ok(Err(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "browse_failed", "message": msg})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
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

#[derive(Serialize)]
pub struct ServerAbout {
    pub version: String,
    pub auth_required: bool,
    pub passphrase_enabled: bool,
    /// Resolved value of `--auth`: `"token"`, `"passphrase"`, or
    /// `"none"`. The frontend Security panel renders the explicit mode
    /// label off this so `--auth=passphrase` is not mislabeled as
    /// `--no-auth`. Derived from `token_manager.is_no_auth()` plus
    /// `login_manager.is_enabled()` because the CLI mode is not
    /// retained in `AppState` (only its effects are).
    pub auth_mode: &'static str,
    pub read_only: bool,
    pub behind_tunnel: bool,
    pub profile: String,
    /// Resolved value of `acp.show_tool_durations` from the active
    /// profile's config. Drives the per-tool elapsed-time label in the
    /// web UI; cross-device since it lives in config.toml.
    pub acp_show_tool_durations: bool,
    /// Resolved value of `acp.queue_drain_mode` from the active
    /// profile's config. Selects how the web composer drains client-side
    /// queued prompts on Stopped: `combined` (default) joins them with
    /// blank lines into a single follow-up; `serial` fires them one at a
    /// time. Cross-device since it lives in config.toml. See #1031.
    pub acp_queue_drain_mode: String,
    /// Resolved value of `acp.max_concurrent_resumes` from the
    /// active profile's config. Upper bound on parallel acp worker
    /// spawns/attaches the reconciler runs on `aoe serve` cold start.
    /// Surfaced so the settings UI shows the current value. See #1088.
    pub acp_max_concurrent_resumes: u32,
    /// Resolved value of `acp.force_end_turn_threshold_secs` from
    /// the active profile's config. Seconds of streaming inactivity
    /// after which the acp web UI offers a "Force end turn" button
    /// to unstick a missed-Stopped spinner. See #1100.
    pub acp_force_end_turn_threshold_secs: u32,
    /// Resolved value of `acp.replay_events` from the active
    /// profile's config. Per-session retention cap on the acp
    /// event log; 0 means unlimited. The web client mirrors this on
    /// its in-memory activity buffer so the rendered transcript
    /// honours the user's chosen ceiling instead of clipping at a
    /// hard-coded constant. See #1111.
    pub acp_replay_events: u32,
    /// `"debug"` when built with `debug_assertions`, `"release"`
    /// otherwise. The web UI renders a "DEV" badge in the topbar
    /// when this is `"debug"` so users can tell concurrently-running
    /// debug (port 8081) and release (port 8080) instances apart at
    /// a glance, including PWA installs where the port disappears
    /// from the window chrome. See #1055.
    pub build_flavor: &'static str,
    /// Content-hashed entry bundle name (`index-<hash>.js`) of the
    /// embedded dashboard build. The client compares this against its
    /// own entry script tag and offers a reload when they differ, so
    /// installed PWAs (which have no refresh affordance) pick up new
    /// dashboard code after the binary updates.
    pub web_build_id: Option<&'static str>,
}

pub async fn get_about(State(state): State<Arc<AppState>>) -> Json<ServerAbout> {
    let auth_required = !state.token_manager.is_no_auth().await;
    let passphrase_enabled = state.login_manager.is_enabled();
    let auth_mode =
        crate::server::resolve_auth_mode(&state.token_manager, &state.login_manager).await;
    let acp_cfg = crate::session::profile_config::resolve_config_or_warn(&state.profile).acp;
    let acp_show_tool_durations = acp_cfg.show_tool_durations;
    let acp_queue_drain_mode = acp_cfg.queue_drain_mode.as_str().to_string();
    let acp_max_concurrent_resumes = acp_cfg.max_concurrent_resumes;
    let acp_force_end_turn_threshold_secs = acp_cfg.force_end_turn_threshold_secs;
    let acp_replay_events = acp_cfg.replay_events;
    Json(ServerAbout {
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_required,
        passphrase_enabled,
        auth_mode,
        read_only: state.read_only,
        behind_tunnel: state.behind_tunnel,
        profile: state.profile.clone(),
        acp_show_tool_durations,
        acp_queue_drain_mode,
        acp_max_concurrent_resumes,
        acp_force_end_turn_threshold_secs,
        acp_replay_events,
        build_flavor: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        web_build_id: crate::server::web_build_id(),
    })
}

// --- Update status ---

/// Web-facing snapshot of `update::check_for_update`. `update_check_mode`
/// mirrors `updates.update_check_mode` so the frontend can hide its banner
/// (mode = `off`) or skip nagging while a background install runs
/// (mode = `auto`) without separately fetching settings.
/// `web_poll_interval_minutes` echoes the configured frontend re-poll cadence
/// so the dashboard doesn't need a second settings round-trip. See #984 and
/// #1140.
#[derive(Serialize)]
pub struct UpdateStatusResponse {
    pub update_check_mode: crate::session::config::UpdateCheckMode,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub web_poll_interval_minutes: u64,
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
    let cfg = crate::session::profile_config::resolve_config_or_warn(&state.profile);
    let current = env!("CARGO_PKG_VERSION").to_string();
    let mode = cfg.updates.update_check_mode;

    if !mode.is_enabled() {
        return Json(UpdateStatusResponse {
            update_check_mode: mode,
            current_version: current,
            latest_version: None,
            update_available: false,
            release_url: None,
            web_poll_interval_minutes: cfg.updates.web_poll_interval_minutes,
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
                web_poll_interval_minutes: cfg.updates.web_poll_interval_minutes,
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
            web_poll_interval_minutes: cfg.updates.web_poll_interval_minutes,
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
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    let name_for_create = body.name.clone();
    match tokio::task::spawn_blocking(move || crate::session::create_profile(&name_for_create))
        .await
    {
        Ok(Ok(())) => {
            crate::server::add_profile_disk_watch(&state, &body.name).await;
            (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "create_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn delete_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    if let Err(e) = validate_profile_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    if name == state.profile {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "active_profile", "message": "Cannot delete the active profile"})),
        )
            .into_response();
    }
    let name_for_delete = name.clone();
    match tokio::task::spawn_blocking(move || crate::session::delete_profile(&name_for_delete))
        .await
    {
        Ok(Ok(())) => {
            crate::server::remove_profile_disk_watch(&state, &name).await;
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "delete_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
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
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    if let Err(e) = validate_profile_name(&body.new_name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
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
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "rename_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
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
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&body.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    let name = body.name;
    match tokio::task::spawn_blocking(move || crate::session::set_default_profile(&name)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "update_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn get_profile_settings(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate_profile_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let profile = crate::session::load_profile_config(&name)?;
        let global = crate::session::Config::load_or_warn();
        let mut val = serde_json::to_value(&profile)?;
        // The `logging` section lives on global Config (no profile
        // override surface yet). Splice it into the response so the
        // settings UI can render its current values from a single
        // GET — without this the dropdowns would reset on every page
        // load even after a successful PATCH.
        if let Some(obj) = val.as_object_mut() {
            obj.insert(
                "logging".to_string(),
                serde_json::to_value(&global.logging)?,
            );
        }
        Ok::<_, anyhow::Error>(val)
    })
    .await;
    match result {
        Ok(Ok(val)) => (StatusCode::OK, Json(val)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "load_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn update_profile_settings(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": e})),
        )
            .into_response();
    }
    // Strip host-execution surfaces (`local_only`) before validation + merge,
    // so a bundled patch keeps its safe leaves and silently drops the
    // local-only ones; they can never become a profile override (#1692).
    strip_local_only(&mut body);
    // Resolve elevation up front. With login disabled (single-user local) the
    // caller is always treated as elevated; with login enabled, only an
    // elevated session may write a requires-elevation field.
    let elevated = if state.login_manager.is_enabled() {
        match session.as_ref() {
            Some(axum::Extension(AuthenticatedSession(id))) => {
                state.login_manager.is_elevated(id).await
            }
            None => false,
        }
    } else {
        true
    };

    // Validate every remaining leaf against the schema (single source of
    // truth, #1692): unknown section/field -> 400, requires-elevation without
    // elevation -> 403 elevation_required (mirrors the path-shape gate's
    // payload so web/src/lib/fetchInterceptor.ts fires the passphrase prompt
    // unchanged, see #1510), bad value -> 400. `description` is accepted here
    // (a profile-only field) but rejected on the global endpoint.
    if let Err(rej) = validate_patch(&body, Scope::Profile, elevated) {
        return reject_response(rej);
    }

    let result = tokio::task::spawn_blocking(move || {
        // The `logging` section is process-global (no profile overrides
        // for v1), so peel it off the patch and write it into the
        // global Config. Everything else stays a per-profile override.
        let mut body = body;
        let logging_patch = body.as_object_mut().and_then(|obj| obj.remove("logging"));
        if let Some(patch) = logging_patch {
            let global = crate::session::Config::load_or_warn();
            let mut current = serde_json::to_value(&global)?;
            if let Some(current_obj) = current.as_object_mut() {
                match current_obj.get_mut("logging") {
                    Some(existing) => {
                        if let (Some(existing_obj), Some(new_obj)) =
                            (existing.as_object_mut(), patch.as_object())
                        {
                            for (k, v) in new_obj {
                                existing_obj.insert(k.clone(), v.clone());
                            }
                        } else {
                            current_obj.insert("logging".to_string(), patch);
                        }
                    }
                    None => {
                        current_obj.insert("logging".to_string(), patch);
                    }
                }
            }
            let global: crate::session::Config = serde_json::from_value(current)?;
            crate::session::save_config(&global)?;
            if let Ok(app_dir) = crate::session::get_app_dir() {
                crate::logging::apply_persisted_config(
                    &global.logging.default_level,
                    &global.logging.targets,
                    &app_dir,
                );
            }
        }

        let config = crate::session::load_profile_config(&name).unwrap_or_default();
        let mut current = serde_json::to_value(&config)?;
        // Apply each validated leaf onto the sparse override object. A null
        // clears the override (revert to inheriting the global); anything else
        // sets it. Sections are created lazily so a single-field patch never
        // wipes its siblings. `description` is a top-level string, handled the
        // same way (set, or removed on null).
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
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "serialize_failed", "message": e.to_string()})),
            )
                .into_response(),
        },
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "update_failed", "message": e.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

// --- Sounds ---

pub async fn list_sounds() -> Json<Vec<String>> {
    Json(crate::sound::list_available_sounds())
}

/// Serve a sound file by name so the acp's browser-side approval
/// player can fetch it from the same origin as the dashboard. The name
/// is validated against `list_available_sounds()` to block path
/// traversal: an attacker who can hit `/api/sounds/file/<x>` cannot
/// read arbitrary disk paths, only files already present in the user's
/// `sounds/` directory.
pub async fn serve_sound_file(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    // The validation step (directory enumeration) stays on the blocking
    // pool because `list_available_sounds` does sync `read_dir`. The
    // file read itself uses `tokio::fs::read` so the larger I/O cost
    // does not block a runtime worker.
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

    fn custom_agents(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(name, command)| ((*name).to_string(), (*command).to_string()))
            .collect()
    }

    #[test]
    fn custom_agent_entries_use_safe_placeholders() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-claude", "ssh -t prod.example claude")]),
            &HashMap::new(),
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
    fn custom_agent_entries_never_serialize_command_values() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-agent", "ssh -t prod.example claude")]),
            &HashMap::new(),
        );

        let json = serde_json::to_string(&entries).unwrap();
        assert!(json.contains("remote-agent"));
        assert!(!json.contains("ssh"));
        assert!(!json.contains("prod.example"));
        assert!(!json.contains("claude"));
    }

    #[test]
    fn serialized_custom_agent_response_contains_no_command_or_detect_as_data() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-agent", "ssh -t prod.example claude")]),
            &HashMap::new(),
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
        let entries = build_custom_agent_infos(&custom, &acp);

        let oc_sp = entries.iter().find(|e| e.name == "oc-sp").unwrap();
        assert!(oc_sp.acp_capable, "agent with a valid acp cmd is capable");
        let plain = entries.iter().find(|e| e.name == "plain").unwrap();
        assert!(!plain.acp_capable, "agent with no acp cmd is tmux-only");
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
        let registry = crate::acp::AgentRegistry::with_defaults();
        let dir = std::path::Path::new("/tmp/aoe-data");
        let (cmd, _) = acp_command_fields(registry.get("aoe-agent"), Some(dir));
        let cmd = cmd.expect("aoe-agent has a registry command");
        assert!(
            !cmd.contains("${aoe_data_dir}"),
            "placeholder must be substituted"
        );
        assert!(cmd.starts_with("/tmp/aoe-data/"));
    }

    #[test]
    fn custom_agent_entries_omit_acp_command_fields() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("oc-sp", "ocp run sp")]),
            &custom_agents(&[("oc-sp", "ocp run sp acp")]),
        );
        let value = serde_json::to_value(&entries).unwrap();
        assert!(value[0].get("acp_command").is_none());
        assert!(value[0].get("acp_args").is_none());
        // The custom acp command must not leak via the new fields.
        assert!(!value.to_string().contains("ocp run sp"));
    }

    #[tokio::test]
    async fn serve_sound_file_rejects_unknown_name() {
        let resp = serve_sound_file(axum::extract::Path("does-not-exist-xyz.wav".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_sound_file_rejects_path_traversal() {
        let resp = serve_sound_file(axum::extract::Path("../../../etc/passwd".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // A NOT_FOUND that somehow still streamed a body would be a
        // worse failure than the wrong status, so assert the body is
        // empty rather than just "not /etc/passwd".
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(body.is_empty(), "unexpected body bytes: {body:?}");
    }
}

//! Misc system endpoints: agents, settings, themes, profiles, filesystem,
//! groups, docker status, devices, about.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::{
    body_requires_elevation, validate_profile_name, ALLOWED_GLOBAL_SETTINGS_SECTIONS,
    ALLOWED_PROFILE_SETTINGS_SECTIONS, COCKPIT_BLOCKED_FIELDS, SESSION_BLOCKED_FIELDS,
};
use crate::server::auth::AuthenticatedSession;

// --- Agents ---

#[derive(Serialize)]
pub struct AgentInfo {
    pub kind: String,
    pub name: String,
    pub binary: String,
    pub host_only: bool,
    pub installed: bool,
    pub install_hint: String,
}

fn build_custom_agent_infos(custom_agents: &HashMap<String, String>) -> Vec<AgentInfo> {
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
        let tools = crate::tmux::AvailableTools::detect();
        let available = tools.available_list();
        let mut agents = crate::agents::AGENTS
            .iter()
            .map(|a| AgentInfo {
                kind: "builtin".to_string(),
                name: a.name.to_string(),
                binary: a.binary.to_string(),
                host_only: a.host_only,
                installed: available.iter().any(|s| s == a.name),
                install_hint: a.install_hint.to_string(),
            })
            .collect::<Vec<_>>();
        agents.extend(build_custom_agent_infos(&custom_agents));
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
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Validate that only allowed sections are being updated. Use the
    // narrower global allowlist here (no `description`, which is profile-only).
    if let Some(obj) = body.as_object() {
        for key in obj.keys() {
            if !ALLOWED_GLOBAL_SETTINGS_SECTIONS.contains(&key.as_str()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "validation_failed",
                        "message": format!("Settings section '{}' is not allowed via the web API.", key)
                    })),
                )
                    .into_response();
            }
        }
    }

    let result = tokio::task::spawn_blocking(move || {
        let config = crate::session::Config::load_or_warn();
        let mut current = serde_json::to_value(&config)?;
        if let (Some(current_obj), Some(update_obj)) = (current.as_object_mut(), body.as_object()) {
            for (key, value) in update_obj {
                let mut value = value.clone();
                // Strip blocked fields from session section
                if key == "session" {
                    if let Some(session_obj) = value.as_object_mut() {
                        for blocked in SESSION_BLOCKED_FIELDS {
                            session_obj.remove(*blocked);
                        }
                    }
                }
                // Strip blocked fields from cockpit section (node_path).
                if key == "cockpit" {
                    if let Some(cockpit_obj) = value.as_object_mut() {
                        for blocked in COCKPIT_BLOCKED_FIELDS {
                            cockpit_obj.remove(*blocked);
                        }
                    }
                }
                current_obj.insert(key.clone(), value);
            }
        }
        let config: crate::session::Config = serde_json::from_value(current)?;
        crate::session::save_config(&config)?;
        Ok::<_, anyhow::Error>(config)
    })
    .await;

    match result {
        Ok(Ok(config)) => {
            // Settings touched [logging]? Apply the new filter live to
            // the daemon + persist runtime_filter so cockpit runners pick
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

// --- Devices ---

pub async fn list_devices(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::server::DeviceInfo>> {
    let devices = state.devices.read().await;
    Json(devices.clone())
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
        return Json(crate::tui::styles::resolve_theme("default"));
    }
    tracing::debug!(theme = %name, "GET /api/themes/{{name}}");
    let resolved = tokio::task::spawn_blocking(move || crate::tui::styles::resolve_theme(&name))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "theme resolve task panicked, falling back to default");
            crate::tui::styles::resolve_theme("default")
        });
    Json(resolved)
}

/// `GET /api/theme/current` returns the resolved theme for the active
/// profile (the picker's current selection). Resolved through
/// `profile_config::resolve_config_or_warn` so per-profile theme
/// overrides land in the right place. Sync work runs in
/// `spawn_blocking`.
pub async fn get_current_theme(
    State(state): State<Arc<AppState>>,
) -> Json<crate::tui::styles::ResolvedTheme> {
    let profile = state.profile.clone();
    tracing::debug!(profile = %profile, "GET /api/theme/current");
    let resolved = tokio::task::spawn_blocking(move || {
        let cfg = crate::session::profile_config::resolve_config_or_warn(&profile);
        let name = if cfg.theme.name.is_empty() {
            "default".to_string()
        } else {
            cfg.theme.name
        };
        crate::tui::styles::resolve_theme(&name)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "current theme resolve task panicked, falling back to default");
        crate::tui::styles::resolve_theme("default")
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
        use crate::containers::ContainerRuntimeInterface;
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
    /// Live value of the `cockpit.enabled` master switch. The settings
    /// UI binds its toggle to this and updates it via
    /// `PATCH /api/cockpit/master`. When true, new sessions for ACP-
    /// capable tools default to cockpit mode; when false, every new
    /// session is tmux.
    pub cockpit_master_enabled: bool,
    /// Resolved value of `cockpit.show_tool_durations` from the active
    /// profile's config. Drives the per-tool elapsed-time label in the
    /// web UI; cross-device since it lives in config.toml.
    pub cockpit_show_tool_durations: bool,
    /// Resolved value of `cockpit.queue_drain_mode` from the active
    /// profile's config. Selects how the web composer drains client-side
    /// queued prompts on Stopped: `combined` (default) joins them with
    /// blank lines into a single follow-up; `serial` fires them one at a
    /// time. Cross-device since it lives in config.toml. See #1031.
    pub cockpit_queue_drain_mode: String,
    /// Resolved value of `cockpit.max_concurrent_resumes` from the
    /// active profile's config. Upper bound on parallel cockpit worker
    /// spawns/attaches the reconciler runs on `aoe serve` cold start.
    /// Surfaced so the settings UI shows the current value. See #1088.
    pub cockpit_max_concurrent_resumes: u32,
    /// Resolved value of `cockpit.force_end_turn_threshold_secs` from
    /// the active profile's config. Seconds of streaming inactivity
    /// after which the cockpit web UI offers a "Force end turn" button
    /// to unstick a missed-Stopped spinner. See #1100.
    pub cockpit_force_end_turn_threshold_secs: u32,
    /// Resolved value of `cockpit.replay_events` from the active
    /// profile's config. Per-session retention cap on the cockpit
    /// event log; 0 means unlimited. The web client mirrors this on
    /// its in-memory activity buffer so the rendered transcript
    /// honours the user's chosen ceiling instead of clipping at a
    /// hard-coded constant. See #1111.
    pub cockpit_replay_events: u32,
    /// `"debug"` when built with `debug_assertions`, `"release"`
    /// otherwise. The web UI renders a "DEV" badge in the topbar
    /// when this is `"debug"` so users can tell concurrently-running
    /// debug (port 8081) and release (port 8080) instances apart at
    /// a glance, including PWA installs where the port disappears
    /// from the window chrome. See #1055.
    pub build_flavor: &'static str,
}

pub async fn get_about(State(state): State<Arc<AppState>>) -> Json<ServerAbout> {
    let auth_required = !state.token_manager.is_no_auth().await;
    let passphrase_enabled = state.login_manager.is_enabled();
    let auth_mode = if auth_required {
        "token"
    } else if passphrase_enabled {
        "passphrase"
    } else {
        "none"
    };
    let cockpit_master_enabled = state
        .cockpit_master_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let cockpit_cfg =
        crate::session::profile_config::resolve_config_or_warn(&state.profile).cockpit;
    let cockpit_show_tool_durations = cockpit_cfg.show_tool_durations;
    let cockpit_queue_drain_mode = cockpit_cfg.queue_drain_mode.as_str().to_string();
    let cockpit_max_concurrent_resumes = cockpit_cfg.max_concurrent_resumes;
    let cockpit_force_end_turn_threshold_secs = cockpit_cfg.force_end_turn_threshold_secs;
    let cockpit_replay_events = cockpit_cfg.replay_events;
    Json(ServerAbout {
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_required,
        passphrase_enabled,
        auth_mode,
        read_only: state.read_only,
        behind_tunnel: state.behind_tunnel,
        profile: state.profile.clone(),
        cockpit_master_enabled,
        cockpit_show_tool_durations,
        cockpit_queue_drain_mode,
        cockpit_max_concurrent_resumes,
        cockpit_force_end_turn_threshold_secs,
        cockpit_replay_events,
        build_flavor: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
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
    match tokio::task::spawn_blocking(move || crate::session::create_profile(&body.name)).await {
        Ok(Ok(())) => (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response(),
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
    match tokio::task::spawn_blocking(move || crate::session::delete_profile(&name)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
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
    match tokio::task::spawn_blocking(move || crate::session::rename_profile(&old, &new)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
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
    // Validate allowed sections. Use the per-profile allowlist here, which
    // is the global list plus `description` (a profile-only field).
    if let Some(obj) = body.as_object() {
        for key in obj.keys() {
            if !ALLOWED_PROFILE_SETTINGS_SECTIONS.contains(&key.as_str()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "validation_failed",
                        "message": format!("Settings section '{}' is not allowed via the web API.", key)
                    })),
                )
                    .into_response();
            }
        }
    }

    // Body-shape elevation gate. The path-shape gate in
    // `requires_elevation` exempts this endpoint so safe preference
    // fields save without a passphrase re-prompt; tamper-surface fields
    // (sandbox image, worktree templates, dangerous session entries)
    // re-impose the elevation requirement here. Mirrors the 403
    // payload shape the path-shape gate returns so the client's
    // existing `elevation_required` handling (web/src/lib/
    // fetchInterceptor.ts) fires `ElevationPrompt` unchanged. See
    // #1510.
    if state.login_manager.is_enabled() && body_requires_elevation(&body) {
        let elevated = match session.as_ref() {
            Some(axum::Extension(AuthenticatedSession(id))) => {
                state.login_manager.is_elevated(id).await
            }
            None => false,
        };
        if !elevated {
            tracing::info!(
                target: "auth.passphrase",
                profile = %name,
                "update_profile_settings: tamper-surface keys in patch without elevation; returning 403"
            );
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "elevation_required",
                    "message": "Re-enter the passphrase to continue"
                })),
            )
                .into_response();
        }
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
        if let (Some(current_obj), Some(update_obj)) = (current.as_object_mut(), body.as_object()) {
            for (key, value) in update_obj {
                let mut value = value.clone();
                if key == "session" {
                    if let Some(session_obj) = value.as_object_mut() {
                        for blocked in SESSION_BLOCKED_FIELDS {
                            session_obj.remove(*blocked);
                        }
                    }
                }
                if key == "cockpit" {
                    if let Some(cockpit_obj) = value.as_object_mut() {
                        for blocked in COCKPIT_BLOCKED_FIELDS {
                            cockpit_obj.remove(*blocked);
                        }
                    }
                }
                // Deep merge within sections so that sending a single field
                // (e.g. {"session": {"yolo_mode_default": true}}) only sets
                // that field as a profile override, preserving other existing
                // overrides instead of replacing the entire section.
                if let Some(existing) = current_obj.get_mut(key) {
                    if let (Some(existing_obj), Some(new_obj)) =
                        (existing.as_object_mut(), value.as_object())
                    {
                        for (k, v) in new_obj {
                            existing_obj.insert(k.clone(), v.clone());
                        }
                    } else {
                        current_obj.insert(key.clone(), value);
                    }
                } else {
                    current_obj.insert(key.clone(), value);
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

/// Serve a sound file by name so the cockpit's browser-side approval
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
        let entries = build_custom_agent_infos(&custom_agents(&[(
            "remote-claude",
            "ssh -t prod.example claude",
        )]));

        assert_eq!(entries.len(), 1);
        let agent = &entries[0];
        assert_eq!(agent.kind, "custom");
        assert_eq!(agent.name, "remote-claude");
        assert_eq!(agent.binary, "remote-claude");
        assert!(!agent.host_only);
        assert!(agent.installed);
        assert_eq!(agent.install_hint, "Configured custom agent");
    }

    #[test]
    fn custom_agent_entries_never_serialize_command_values() {
        let entries = build_custom_agent_infos(&custom_agents(&[(
            "remote-agent",
            "ssh -t prod.example claude",
        )]));

        let json = serde_json::to_string(&entries).unwrap();
        assert!(json.contains("remote-agent"));
        assert!(!json.contains("ssh"));
        assert!(!json.contains("prod.example"));
        assert!(!json.contains("claude"));
    }

    #[test]
    fn serialized_custom_agent_response_contains_no_command_or_detect_as_data() {
        let entries = build_custom_agent_infos(&custom_agents(&[(
            "remote-agent",
            "ssh -t prod.example claude",
        )]));
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
        let entries = build_custom_agent_infos(&custom_agents(&[
            ("", "codex"),
            ("empty-command", ""),
            ("   ", "codex"),
            ("whitespace-command", "   "),
            ("claude", "ssh -t prod.example claude"),
            ("remote-codex", "ssh -t prod.example codex"),
        ]));

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["remote-codex"]);
        assert!(entries.iter().all(|entry| entry.kind == "custom"));
    }

    #[test]
    fn custom_agent_entries_are_sorted_by_name() {
        let entries = build_custom_agent_infos(&custom_agents(&[
            ("zeta", "zeta-cmd"),
            ("alpha", "alpha-cmd"),
            ("middle", "middle-cmd"),
        ]));

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zeta"]);
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

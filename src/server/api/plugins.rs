//! Plugin management REST API: list plugins and enable/disable them. The web
//! twin of `aoe plugin`.
//!
//! The enable/disable toggle is a mutation that runs on the host, so it
//! requires read-write mode AND an elevated session when login is enabled,
//! mirroring the requires-elevation settings fields.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use crate::plugin;
use crate::plugin::install::OperationLog;
use crate::server::auth::{handler_elevated, AuthenticatedSession, LoopbackTrusted};

const CAP_COMPOSER_READ: &str = "composer.read";
const CAP_BROWSER_MICROPHONE: &str = "browser.microphone";
pub const BROWSER_VOICE_BODY_LIMIT_BYTES: usize = 12 * 1024 * 1024;
const MAX_BROWSER_VOICE_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const MAX_BROWSER_VOICE_AUDIO_BASE64_BYTES: usize = MAX_BROWSER_VOICE_AUDIO_BYTES / 3 * 4 + 4;
const MAX_BROWSER_VOICE_DURATION_MS: u64 = 120_000;
const MAX_BROWSER_VOICE_PARAMS_BYTES: usize = 64 * 1024;
const MAX_PLUGIN_ACTION_METHOD_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_CAPTURE_ID_BYTES: usize = 128;
const MAX_AUDIO_MIME_TYPE_BYTES: usize = 128;

fn error_response(status: StatusCode, code: &str, message: String) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

/// Resolve the read-only and elevation gates shared by every mutation.
/// Elevation goes through `handler_elevated`, so a loopback-trusted
/// caller passes without a session (#2610): the loopback bypass paths
/// never insert `AuthenticatedSession`, and treating that as
/// not-elevated made these mutations unreachable from localhost.
async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
    loopback_trusted: bool,
) -> Result<(), Response> {
    if state.read_only {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        ));
    }
    if !handler_elevated(state, session, loopback_trusted).await {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue".into(),
        ));
    }
    Ok(())
}

/// `GET /api/plugins`: every known plugin plus load errors.
pub async fn list_plugins() -> Json<serde_json::Value> {
    let registry = plugin::registry();
    Json(json!({
        "plugins": registry.all().iter().map(|p| p.view()).collect::<Vec<_>>(),
        "load_errors": registry.load_errors(),
    }))
}

/// Resolve a plugin's declared `icon_asset` (repository-relative, already
/// `screenshot_path_ok`-checked) against its install directory, refusing to
/// serve anything outside that directory. Both `dir` and the joined path are
/// canonicalized so a symlink or `..` segment cannot escape containment; the
/// caller passes the already-loaded `dir`/`rel` rather than this function
/// touching the registry, so it is plain path logic and testable without a
/// running plugin host.
fn resolve_plugin_icon_path(dir: &std::path::Path, rel: &str) -> Option<PathBuf> {
    if !aoe_plugin_api::screenshot_path_ok(rel) {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let target = root.join(rel).canonicalize().ok()?;
    target.starts_with(&root).then_some(target)
}

fn content_type_for_icon(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// `GET /api/plugins/{id}/icon`: stream an installed plugin's `icon_asset`
/// from its install directory. Mirrors `serve_sound_file`'s allowlist-then-read
/// shape: the manifest path is re-validated and re-joined against the
/// plugin's own directory rather than trusted from a cached URL. A builtin
/// (no install directory) or a plugin with no `icon_asset` 404s.
pub async fn serve_plugin_icon(Path(id): Path<String>) -> Response {
    let registry = plugin::registry();
    let Some(plugin) = registry.all().iter().find(|p| p.id() == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (Some(dir), Some(rel)) = (plugin.dir.clone(), plugin.manifest.icon_asset.clone()) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let resolved = tokio::task::spawn_blocking(move || resolve_plugin_icon_path(&dir, &rel)).await;
    let path = match resolved {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(content_type) = content_type_for_icon(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
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

/// One active plugin command, normalized for the dashboard command palette and
/// keymap: the namespaced `fqid`, its declared keybind chords, and its optional
/// client-executed `action`. The web binds and renders these without parsing
/// raw manifests.
#[derive(Serialize)]
struct PluginCommandView {
    fqid: String,
    plugin_id: String,
    id: String,
    title: String,
    description: String,
    keybinds: Vec<String>,
    action: Option<aoe_plugin_api::ClientAction>,
}

/// `GET /api/plugins/commands`: active plugins' contributed commands, each with
/// the chords bound to it and its client action. Reads the registry (manifests),
/// not workers, so it is safe in read-only mode.
pub async fn plugin_commands() -> Json<serde_json::Value> {
    let registry = plugin::registry();
    let mut commands = Vec::new();
    for p in registry.active() {
        let plugin_id = p.id().to_string();
        for c in &p.manifest.commands {
            let fqid = format!("plugin.{plugin_id}.{}", c.id);
            let keybinds = p
                .manifest
                .keybinds
                .iter()
                .filter(|kb| kb.command == c.id || kb.command == fqid)
                .map(|kb| kb.key.clone())
                .collect();
            commands.push(PluginCommandView {
                fqid: fqid.clone(),
                plugin_id: plugin_id.clone(),
                id: c.id.clone(),
                title: c.title.clone(),
                description: c.description.clone(),
                keybinds,
                action: c.action.clone(),
            });
        }
    }
    Json(json!({ "commands": commands }))
}

/// `GET /api/plugins/ui-state`: the plugin host's aggregated UI-state snapshot
/// (the slots workers have pushed, plus the notification ring). Empty when no
/// host is running (read-only mode, or a TUI-only build with no daemon). The
/// dashboard polls this alongside `/api/sessions` and renders each slot itself.
pub async fn plugin_ui_state(
    State(state): State<std::sync::Arc<AppState>>,
) -> Json<serde_json::Value> {
    let empty = || json!({ "entries": [], "notifications": [] });
    match state.plugin_host.as_ref().map(|h| h.ui_snapshot()) {
        Some(snapshot) => Json(serde_json::to_value(snapshot).unwrap_or_else(|e| {
            // Serializing the snapshot should never fail; if it somehow does,
            // keep the response shape stable rather than returning JSON null.
            tracing::warn!(target: "serve.api", "failed to serialize plugin UI snapshot: {e}");
            empty()
        })),
        None => Json(empty()),
    }
}

/// `GET /api/plugins/updates`: which installed external plugins have an update
/// available. An explicit, on-demand network check (the dashboard "Check for
/// updates" button), kept off the always-on `GET /api/plugins` list path so a
/// settings render never blocks on git/network. Allowed in read-only mode: it
/// reads remote state and mutates nothing.
pub async fn plugin_updates() -> Json<serde_json::Value> {
    Json(json!({ "updates": plugin::update_check::outdated().await }))
}

#[derive(Deserialize)]
pub struct DiscoverQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /api/plugins/discover?q=`: search the `aoe-plugin` GitHub topic. The
/// dashboard "Search GitHub" button. Browse-only: the dashboard has no install
/// path (capability approval needs a terminal), so each result carries an
/// `install_command` the user copies. On a GitHub failure (notably the
/// unauthenticated search rate limit) the message is returned for the UI to
/// show, rather than a generic 500.
pub async fn plugin_discover(Query(query): Query<DiscoverQuery>) -> Response {
    match plugin::discover::discover(query.q.as_deref()).await {
        Ok(results) => Json(json!({ "results": results })).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, "discover_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct DetailsQuery {
    pub source: String,
}

/// `GET /api/plugins/details?source=gh:owner/repo`: the on-demand detail for one
/// plugin source (manifest fields + release tags) backing the dashboard detail
/// modal. Allowed in read-only mode; reads remote state and mutates nothing.
pub async fn plugin_details(Query(query): Query<DetailsQuery>) -> Response {
    match plugin::discover::details(&query.source).await {
        Ok(detail) => Json(detail).into_response(),
        // `details()` only hard-errors on an invalid / unsupported `source`; a
        // GitHub fetch failure is reported in-band (manifest_error / empty
        // release tags), so a hard error here is bad client input, not an
        // upstream outage.
        Err(e) => error_response(StatusCode::BAD_REQUEST, "invalid_source", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct PluginActionBody {
    /// The worker method to invoke (the plugin names it in its UI action
    /// payload, e.g. `github.refresh`).
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// The session whose UI fired the action, if any. The host reads the
    /// baseline revision for this `(plugin, session)` scope so the dashboard
    /// waits only for that scope's re-pushed state. It is merged into
    /// `params.session_id` before forwarding to the worker.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBrowserVoiceInputBody {
    /// The worker method to invoke after the browser host captures audio.
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// The session whose composer action started recording.
    pub session_id: String,
    /// Opaque id generated by the initiating browser. The worker echoes this
    /// in a draft operation; draft text and selection stay browser-local.
    pub capture_id: String,
    pub audio: BrowserVoiceAudioUpload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserVoiceAudioUpload {
    #[serde(default)]
    pub mime_type: Option<String>,
    pub duration_ms: u64,
    pub data_base64: String,
}

/// `POST /api/plugins/{id}/action`: forward a dashboard UI action to the
/// plugin's worker as a fire-and-forget JSON-RPC notification.
/// The worker is the trust boundary: it acts only on methods it implements and
/// ignores the rest, so this never waits for or returns a worker result.
///
/// Gated on read-write mode only, not elevation. Unlike enable/disable, a UI
/// action does not mutate host-managed state (config, registry, grants,
/// lockfile) and grants no new host capability, so it does not warrant the
/// passphrase step-up, the same reasoning as `update_theme` in `system.rs`.
/// A routine `github.refresh` should not prompt for the passphrase.
pub async fn invoke_plugin_action(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginActionBody>,
) -> Response {
    if state.read_only {
        return error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        );
    }
    let Some(host) = state.plugin_host.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_host",
            "Plugin host is not running".into(),
        );
    };
    // Read the UI revision before forwarding, not the value the dashboard
    // last polled: that one is stale, so an unrelated push between the last poll
    // and this click would already exceed it and clear the spinner before the
    // worker has done anything. Scoped to the firing UI's session so another
    // session's activity cannot move it. The dashboard holds the spinner until
    // this scope's revision moves off the baseline.
    let baseline_revision = host.ui_revision(&id, body.session_id.as_deref());
    // Forward the firing UI's session to the worker so a per-session action
    // (e.g. github.refresh) can scope its work to that session instead of every
    // one. Merged into the params object; a worker that does not use it ignores
    // it (the honest-plugin model).
    let mut params = body.params;
    strip_composer_snapshot_without_capability(&id, &mut params);
    merge_session_id(&mut params, body.session_id.as_deref());
    if host.notify_worker(&id, &body.method, params).await {
        (
            StatusCode::ACCEPTED,
            Json(json!({ "ok": true, "baseline_revision": baseline_revision })),
        )
            .into_response()
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            "no_worker",
            format!("No running worker for plugin {id}"),
        )
    }
}

/// `POST /api/plugins/{id}/browser-voice-input`: accept audio captured by the
/// trusted dashboard/PWA host and forward it to the plugin worker that declared
/// a `composer-action` browser voice input. The plugin never runs browser JS;
/// this endpoint is the browser capability bridge.
pub async fn plugin_browser_voice_input(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginBrowserVoiceInputBody>,
) -> Response {
    if state.read_only {
        return error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        );
    }
    if !valid_plugin_action_method(&body.method) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_method",
            "Plugin action method must be 1 to 128 safe ASCII characters".into(),
        );
    }
    if !valid_bounded_identifier(&body.session_id, MAX_SESSION_ID_BYTES) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_session_id",
            "Browser voice input requires a valid session id".into(),
        );
    }
    if !valid_capture_id(&body.capture_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_capture_id",
            "Browser voice input requires a valid capture id".into(),
        );
    }
    if !body.params.is_null() && !body.params.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "Browser voice input params must be an object".into(),
        );
    }
    if serde_json::to_vec(&body.params)
        .map_or(true, |bytes| bytes.len() > MAX_BROWSER_VOICE_PARAMS_BYTES)
    {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "params_too_large",
            format!(
                "Browser voice input params exceed {} KiB",
                MAX_BROWSER_VOICE_PARAMS_BYTES / 1024
            ),
        );
    }
    let Some(host) = state.plugin_host.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_host",
            "Plugin host is not running".into(),
        );
    };
    if !active_plugin_has_capability(&id, CAP_BROWSER_MICROPHONE) {
        return error_response(
            StatusCode::FORBIDDEN,
            "capability_required",
            format!("Plugin {id} is not allowed to use browser microphone capture"),
        );
    }
    if !declares_active_browser_voice_action(host, &id, &body.session_id, &body.method) {
        return error_response(
            StatusCode::FORBIDDEN,
            "browser_action_required",
            "No active browser voice action matches this plugin, session, and method".into(),
        );
    }
    let mime_type = body
        .audio
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("audio/webm");
    if mime_type.len() > MAX_AUDIO_MIME_TYPE_BYTES || !is_allowed_audio_content_type(mime_type) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_audio_type",
            "Unsupported browser voice input audio type".into(),
        );
    }
    if body.audio.duration_ms == 0 || body.audio.duration_ms > MAX_BROWSER_VOICE_DURATION_MS {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_audio_duration",
            format!(
                "Browser voice input duration must be between 1 ms and {} ms",
                MAX_BROWSER_VOICE_DURATION_MS
            ),
        );
    }
    if body.audio.data_base64.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "empty_audio",
            "Browser voice input audio is empty".into(),
        );
    }
    if body.audio.data_base64.len() > MAX_BROWSER_VOICE_AUDIO_BASE64_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_large",
            format!(
                "Browser voice input audio exceeds {} MiB",
                MAX_BROWSER_VOICE_AUDIO_BYTES / 1024 / 1024
            ),
        );
    }
    let decoded_len = match BASE64_STANDARD.decode(&body.audio.data_base64) {
        Ok(bytes) if bytes.is_empty() => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "empty_audio",
                "Browser voice input audio is empty".into(),
            );
        }
        Ok(bytes) => bytes.len(),
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_audio",
                "Browser voice input audio is not valid base64".into(),
            );
        }
    };
    if decoded_len > MAX_BROWSER_VOICE_AUDIO_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_large",
            format!(
                "Browser voice input audio exceeds {} MiB",
                MAX_BROWSER_VOICE_AUDIO_BYTES / 1024 / 1024
            ),
        );
    }

    let baseline_revision = host.ui_revision(&id, Some(&body.session_id));
    let mut params = body.params;
    strip_composer_snapshot_without_capability(&id, &mut params);
    merge_session_id(&mut params, Some(&body.session_id));
    merge_browser_voice_audio(
        &mut params,
        &body.capture_id,
        mime_type,
        decoded_len,
        body.audio.duration_ms,
        body.audio.data_base64,
    );

    if host.notify_worker(&id, &body.method, params).await {
        (
            StatusCode::ACCEPTED,
            Json(json!({ "ok": true, "baseline_revision": baseline_revision })),
        )
            .into_response()
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            "no_worker",
            format!("No running worker for plugin {id}"),
        )
    }
}

fn strip_composer_snapshot_without_capability(plugin_id: &str, params: &mut serde_json::Value) {
    let can_read = active_plugin_has_capability(plugin_id, CAP_COMPOSER_READ);
    if can_read {
        return;
    }
    if let serde_json::Value::Object(map) = params {
        map.remove("composer");
    }
}

fn active_plugin_has_capability(plugin_id: &str, capability: &str) -> bool {
    plugin::registry()
        .get(plugin_id)
        .filter(|p| p.active())
        .is_some_and(|p| {
            p.manifest
                .capabilities
                .iter()
                .any(|cap| cap.as_str() == capability)
        })
}

fn declares_active_browser_voice_action(
    host: &crate::plugin::host::PluginHost,
    plugin_id: &str,
    session_id: &str,
    method: &str,
) -> bool {
    host.ui_snapshot().entries.iter().any(|entry| {
        entry.plugin_id == plugin_id
            && entry.slot == aoe_plugin_api::UiSlot::ComposerAction
            && entry.session_id.as_deref() == Some(session_id)
            && entry
                .payload
                .get("method")
                .and_then(serde_json::Value::as_str)
                == Some(method)
            && entry
                .payload
                .get("browser_action")
                .and_then(|action| action.get("kind"))
                .and_then(serde_json::Value::as_str)
                == Some("voice-input")
    })
}

fn valid_plugin_action_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= MAX_PLUGIN_ACTION_METHOD_BYTES
        && method.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
}

fn valid_bounded_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn valid_capture_id(capture_id: &str) -> bool {
    valid_bounded_identifier(capture_id, MAX_CAPTURE_ID_BYTES)
        && capture_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn merge_session_id(params: &mut serde_json::Value, session_id: Option<&str>) {
    let Some(sid) = session_id else {
        return;
    };
    match params {
        serde_json::Value::Object(map) => {
            map.insert(
                "session_id".into(),
                serde_json::Value::String(sid.to_string()),
            );
        }
        serde_json::Value::Null => *params = json!({ "session_id": sid }),
        _ => {}
    }
}

fn merge_browser_voice_audio(
    params: &mut serde_json::Value,
    capture_id: &str,
    mime_type: &str,
    byte_len: usize,
    duration_ms: u64,
    data_base64: String,
) {
    if !params.is_object() {
        *params = json!({});
    }
    if let serde_json::Value::Object(map) = params {
        map.insert(
            "browser".into(),
            json!({
                "action": "voice-input",
                "capture_id": capture_id,
                "audio": {
                    "mime_type": mime_type,
                    "bytes": byte_len,
                    "duration_ms": duration_ms,
                    "data_base64": data_base64,
                }
            }),
        );
    }
}

fn is_allowed_audio_content_type(content_type: &str) -> bool {
    if content_type.is_empty()
        || content_type.len() > MAX_AUDIO_MIME_TYPE_BYTES
        || !content_type
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return false;
    }
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "audio/webm"
            | "audio/ogg"
            | "audio/mpeg"
            | "audio/mp3"
            | "audio/mp4"
            | "audio/wav"
            | "audio/x-wav"
            | "audio/wave"
            | "audio/flac"
    )
}

/// `GET /api/plugins/{id}/update/preview`: classify the available update for one
/// installed external plugin (no_update / safe_update / consent_required) and,
/// when consent is required, return the structured disclosure the dashboard and
/// TUI render. Gated on read-write mode only, NOT elevation: it mutates no host
/// state and it powers the approval UI, so a non-elevated session must be able
/// to fetch the capability diff before deciding (elevation is required on the
/// actual apply). Network failures (no release, dead remote) surface as a 502.
pub async fn plugin_update_preview(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.read_only {
        return error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        );
    }
    match plugin::install::preview_update(&id).await {
        Ok(preview) => Json(preview).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, "preview_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct ApplyUpdateBody {
    /// The fingerprint the user approved, from the preview. Pins the apply to
    /// exactly what was shown: if the remote moved since, the apply is refused.
    #[serde(default)]
    pub expected_fingerprint: Option<String>,
}

/// `POST /api/plugins/{id}/update/apply`: apply an update the user approved in
/// the dashboard, granting whatever the fetched manifest declares. A privileged
/// host mutation (it can expand the capability set and run build steps), so it
/// is gated on read-write mode AND elevation, like enable/disable. Runs as a
/// host-side job so the build is observable; returns a `job_id` the dashboard
/// polls for the live log. A fingerprint mismatch (the remote moved since the
/// preview) surfaces as a failed job, which the UI recovers from by
/// re-previewing.
pub async fn apply_plugin_update(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Path(id): Path<String>,
    Json(body): Json<ApplyUpdateBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let plugin_id = id.clone();
    let fingerprint = body.expected_fingerprint;
    start_job(state, PluginJobKind::Update, id, move |log| async move {
        plugin::install::apply_update(&plugin_id, fingerprint, &log)
            .await
            .map(|_| ())
    })
}

#[derive(Deserialize)]
pub struct DismissUpdateBody {
    /// The fingerprint of the update the user declined, from the preview.
    pub fingerprint: String,
}

/// `POST /api/plugins/{id}/update/dismiss`: record that the user declined an
/// available update, so the popup and the auto-update notification stop nagging
/// until the next version. Mutates host config and suppresses a security
/// signal, so it is gated like apply (read-write + elevation).
pub async fn dismiss_plugin_update(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Path(id): Path<String>,
    Json(body): Json<DismissUpdateBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        plugin::install::dismiss_update(&id, &body.fingerprint)
    })
    .await;
    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

/// `POST /api/plugins/{id}/enabled`
pub async fn set_plugin_enabled(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabledBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let result =
        tokio::task::spawn_blocking(move || plugin::install::set_enabled(&id, body.enabled)).await;
    match result {
        Ok(Ok(())) => {
            // set_enabled reloaded the global registry on disk; reconcile the
            // live host so enabling launches the worker and disabling tears it
            // down, without waiting for a full daemon restart. reconcile is
            // async, so it runs here after the sync spawn_blocking returns,
            // never inside it.
            if let Some(host) = state.plugin_host.clone() {
                host.reconcile(&crate::plugin::registry()).await;
            }
            list_plugins().await.into_response()
        }
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

// Plugin lifecycle jobs: install, update, and uninstall.

use std::sync::atomic::{AtomicBool, Ordering};

/// A host-side plugin lifecycle operation the dashboard started and tails. The
/// daemon owns the work; the browser polls `GET /api/plugins/jobs/{id}` for
/// status plus the live log tail.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginJobKind {
    Install,
    Update,
    Uninstall,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PluginJobStatus {
    Running,
    Succeeded,
    Failed { error: String },
}

#[derive(Clone, Serialize)]
pub struct PluginJob {
    pub id: String,
    pub kind: PluginJobKind,
    /// What is being operated on: a source slug for install, a plugin id else.
    pub target: String,
    pub status: PluginJobStatus,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// On-disk log file; never serialized (read via the tail endpoint instead).
    #[serde(skip)]
    log_path: PathBuf,
}

/// Drop finished jobs and their log files older than this when a new job
/// starts. A dashboard polls a job for seconds to minutes; an hour is a wide
/// margin that bounds the in-memory map and the on-disk logs over a long-lived
/// daemon.
const FINISHED_JOB_TTL_SECS: i64 = 3600;

/// In-memory registry of plugin lifecycle jobs. Dies with the daemon: a job
/// running at shutdown is gone, but its on-disk log survives so a tail after a
/// restart still shows what happened, just without live status.
// ponytail: in-memory only; a persisted job table would need process
// supervision and orphaned-build recovery to mean anything. Add that only if
// restart-survival of in-flight jobs is ever required.
pub struct PluginJobRegistry {
    jobs: Mutex<HashMap<String, PluginJob>>,
    /// At most one lifecycle mutation runs at a time. Config + lockfile writes
    /// and in-place tree mutations are not concurrency-safe, so a second start
    /// is rejected with 409 rather than queued (a queued mutation can go stale
    /// before it runs).
    active: AtomicBool,
}

impl Default for PluginJobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginJobRegistry {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            active: AtomicBool::new(false),
        }
    }

    /// Begin a job if no other lifecycle mutation is active. Returns the job id
    /// and its log path, or `None` if one is already running.
    fn begin(&self, kind: PluginJobKind, target: String) -> Option<(String, PathBuf)> {
        if self
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let log_path = match plugin::plugins_dir() {
            Ok(dir) => dir
                .join("jobs")
                .join(format!("{}.log", uuid::Uuid::new_v4())),
            Err(_) => {
                self.active.store(false, Ordering::SeqCst);
                return None;
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        self.prune();
        let job = PluginJob {
            id: id.clone(),
            kind,
            target,
            status: PluginJobStatus::Running,
            started_at: chrono::Utc::now().timestamp(),
            finished_at: None,
            log_path: log_path.clone(),
        };
        self.jobs.lock().unwrap().insert(id.clone(), job);
        Some((id, log_path))
    }

    /// Mark a job done and release the single-active guard.
    fn finish(&self, id: &str, result: anyhow::Result<()>) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.finished_at = Some(chrono::Utc::now().timestamp());
            job.status = match result {
                Ok(()) => PluginJobStatus::Succeeded,
                Err(e) => PluginJobStatus::Failed {
                    error: format!("{e:#}"),
                },
            };
        }
        self.active.store(false, Ordering::SeqCst);
    }

    pub fn get(&self, id: &str) -> Option<PluginJob> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    /// Drop finished jobs older than the TTL and remove their log files.
    fn prune(&self) {
        let cutoff = chrono::Utc::now().timestamp() - FINISHED_JOB_TTL_SECS;
        self.jobs.lock().unwrap().retain(|_, job| {
            let stale = job.finished_at.is_some_and(|t| t < cutoff);
            if stale {
                let _ = std::fs::remove_file(&job.log_path);
            }
            !stale
        });
    }
}

/// Begin a lifecycle job, spawn its work, and return `202 { job_id }`. Returns
/// `409` when another lifecycle mutation is already running. The work runs in a
/// detached task; its build output and host-side progress lines land in the job
/// log file, which the dashboard tails via `plugin_job_status`.
// ponytail: install/update run their (synchronous) build inside this async
// task, parking one runtime worker for the build's duration. The single-active
// guard caps that at one parked worker; switch to a dedicated blocking thread
// only if that ever matters.
fn start_job<F, Fut>(
    state: std::sync::Arc<AppState>,
    kind: PluginJobKind,
    target: String,
    run: F,
) -> Response
where
    F: FnOnce(OperationLog) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let Some((job_id, log_path)) = state.plugin_jobs.begin(kind, target) else {
        return error_response(
            StatusCode::CONFLICT,
            "plugin_job_active",
            "Another plugin operation is already running".into(),
        );
    };
    let jobs = state.plugin_jobs.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        let result = match OperationLog::file(&log_path) {
            Ok(log) => run(log).await,
            Err(e) => Err(e),
        };
        jobs.finish(&id, result);
    });
    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id }))).into_response()
}

#[derive(Deserialize)]
pub struct InstallPreviewBody {
    pub source: String,
}

/// `POST /api/plugins/install/preview`: classify a `gh:` install candidate and
/// return the capability / build / UI disclosure the dashboard renders before
/// the user approves. Read-write only, NOT elevation: it mutates nothing and
/// powers the approval UI (elevation is required on the actual install).
pub async fn preview_plugin_install(
    State(state): State<std::sync::Arc<AppState>>,
    Json(body): Json<InstallPreviewBody>,
) -> Response {
    if state.read_only {
        return error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        );
    }
    match plugin::install::preview_install(&body.source).await {
        Ok(consent) => Json(consent).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, "preview_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct StartInstallBody {
    pub source: String,
    /// The fingerprint the user approved, from the preview. Pins the install to
    /// exactly what was shown.
    pub expected_fingerprint: String,
}

/// `POST /api/plugins/install`: start a host-side install job for a `gh:` source
/// the user approved in the dashboard. Read-write + elevation, like update
/// apply. Returns a `job_id` to poll; `409` if another lifecycle job is running.
pub async fn start_plugin_install(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Json(body): Json<StartInstallBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let source = body.source.clone();
    let fingerprint = body.expected_fingerprint;
    start_job(
        state,
        PluginJobKind::Install,
        source.clone(),
        move |log| async move {
            plugin::install::apply_install(&source, &fingerprint, &log)
                .await
                .map(|_| ())
        },
    )
}

/// `POST /api/plugins/{id}/uninstall`: start a host-side uninstall job. Removes
/// the plugin's tree, config entry, and lockfile entry. Read-write + elevation;
/// returns a `job_id` to poll; `409` if another lifecycle job is running.
pub async fn start_plugin_uninstall(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let plugin_id = id.clone();
    start_job(state, PluginJobKind::Uninstall, id, move |log| async move {
        // Uninstall is synchronous filesystem work; run it off the async task so
        // it never parks a runtime worker.
        match tokio::task::spawn_blocking(move || {
            plugin::install::uninstall_logged(&plugin_id, &log)
        })
        .await
        {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("uninstall task failed: {e}")),
        }
    })
}

#[derive(Deserialize)]
pub struct JobLogQuery {
    /// Trailing lines to return; clamped to [1, 2000], default 200.
    pub tail: Option<usize>,
}

/// `GET /api/plugins/jobs/{job_id}`: a lifecycle job's status plus a bounded
/// tail of its host-side log. Polled by the dashboard progress modal. Reads job
/// state only, so no elevation; the global auth middleware still applies.
pub async fn plugin_job_status(
    State(state): State<std::sync::Arc<AppState>>,
    Path(job_id): Path<String>,
    Query(q): Query<JobLogQuery>,
) -> Response {
    let Some(job) = state.plugin_jobs.get(&job_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "job_not_found",
            format!("No plugin job {job_id}"),
        );
    };
    let tail = q.tail.unwrap_or(200).clamp(1, 2000);
    let log_path = job.log_path.clone();
    let read = tokio::task::spawn_blocking(move || {
        crate::server::api::acp::read_log_tail(&log_path, tail)
    })
    .await;
    match read {
        Ok(Ok((lines, truncated, exists))) => Json(json!({
            "job": job,
            "log": {
                "exists": exists,
                "tail": lines.join("\n"),
                "lines_returned": lines.len(),
                "truncated": truncated,
            }
        }))
        .into_response(),
        Ok(Err(e)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "log_read_failed",
            format!("{e}"),
        ),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{update_config, CapabilityGrant, Config, PluginConfig};
    use aoe_plugin_api::PluginManifest;

    #[test]
    fn registry_allows_one_active_job_then_releases_on_finish() {
        let reg = PluginJobRegistry::new();
        let (id1, _) = reg
            .begin(PluginJobKind::Install, "gh:a/b".into())
            .expect("first job begins");
        // A second lifecycle mutation is rejected while one is active: config
        // and lockfile writes are not concurrency-safe.
        assert!(
            reg.begin(PluginJobKind::Uninstall, "x".into()).is_none(),
            "second job rejected while one is active"
        );
        assert!(matches!(
            reg.get(&id1).unwrap().status,
            PluginJobStatus::Running
        ));

        reg.finish(&id1, Ok(()));
        assert!(matches!(
            reg.get(&id1).unwrap().status,
            PluginJobStatus::Succeeded
        ));

        // The guard is released, so a new job can begin and a failure records
        // its message.
        let (id2, _) = reg
            .begin(PluginJobKind::Update, "gh:c/d".into())
            .expect("job begins after the prior one finished");
        reg.finish(&id2, Err(anyhow::anyhow!("boom")));
        match reg.get(&id2).unwrap().status {
            PluginJobStatus::Failed { error } => assert!(error.contains("boom"), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_plugin_icon_path_serves_a_file_inside_the_install_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("icon.png"), b"fake png bytes").unwrap();
        let resolved = resolve_plugin_icon_path(dir.path(), "icon.png").expect("resolves");
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("icon.png")
        );
    }

    #[test]
    fn resolve_plugin_icon_path_rejects_traversal_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("icon.png"), b"x").unwrap();
        // "../secret.png" is rejected by screenshot_path_ok's shape check
        // alone, before any filesystem access, so no sibling file is needed
        // to prove containment holds.
        for bad in [
            "../secret.png",
            "/etc/passwd.png",
            "icon.svg",
            "missing.png",
        ] {
            assert!(
                resolve_plugin_icon_path(dir.path(), bad).is_none(),
                "{bad:?} should not resolve"
            );
        }
    }

    #[test]
    fn content_type_for_icon_covers_raster_extensions_only() {
        assert_eq!(
            content_type_for_icon(std::path::Path::new("a.png")),
            Some("image/png")
        );
        assert_eq!(
            content_type_for_icon(std::path::Path::new("a.jpg")),
            Some("image/jpeg")
        );
        assert_eq!(
            content_type_for_icon(std::path::Path::new("a.jpeg")),
            Some("image/jpeg")
        );
        assert_eq!(
            content_type_for_icon(std::path::Path::new("a.gif")),
            Some("image/gif")
        );
        assert_eq!(
            content_type_for_icon(std::path::Path::new("a.webp")),
            Some("image/webp")
        );
        assert_eq!(content_type_for_icon(std::path::Path::new("a.svg")), None);
        assert_eq!(content_type_for_icon(std::path::Path::new("a")), None);
    }

    /// Reloads the process-global plugin registry on Drop. Ordered as a field
    /// AFTER `AppDirEnvGuard::_env` so it runs once the env has been restored
    /// (matching the pre-consolidation Drop, which reloaded after restoring).
    struct ReloadRegistryOnDrop;

    impl Drop for ReloadRegistryOnDrop {
        fn drop(&mut self) {
            // Re-acquire the process-global env lock (released when the sibling
            // `_env` field dropped just before this) so the registry reload
            // reads a HOME/XDG that no peer test is concurrently mutating.
            // `reload_registry` resolves the app dir from those vars, so an
            // unlocked reload here could otherwise read a racing test's dirs.
            let _lock = crate::session::test_support::EnvGuard::unset(&[]);
            crate::plugin::reload_registry();
        }
    }

    struct AppDirEnvGuard {
        // Field drop order is load-bearing: `_env` restores HOME / XDG /
        // USERPROFILE (and releases the shared env lock) first, then
        // `_reload` reloads the registry against the restored dirs, then
        // `_temp` deletes the tempdir. `_env` also holds the process-global
        // env lock for the guard's whole lifetime (issues #2864, #2600).
        _env: crate::session::test_support::EnvGuard,
        _reload: ReloadRegistryOnDrop,
        _temp: tempfile::TempDir,
    }

    impl AppDirEnvGuard {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let env = crate::session::test_support::EnvGuard::set(&[
                ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
                ("HOME", temp.path().to_path_buf()),
                ("USERPROFILE", temp.path().to_path_buf()),
            ]);
            crate::plugin::reload_registry();
            Self {
                _env: env,
                _reload: ReloadRegistryOnDrop,
                _temp: temp,
            }
        }
    }

    fn write_plugin_manifest(dir_name: &str, id: &str, capabilities: &[&str]) -> String {
        let capabilities = capabilities
            .iter()
            .map(|cap| format!("{cap:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            r#"
id = "{id}"
name = "Test Plugin"
version = "1.0.0"
api_version = 8
capabilities = [{capabilities}]

[[ui]]
slot = "composer-action"
id = "voice"
"#
        );
        let dir = crate::plugin::plugins_dir()
            .expect("plugins dir")
            .join(dir_name);
        std::fs::create_dir_all(&dir).expect("create plugin dir");
        std::fs::write(dir.join("aoe-plugin.toml"), &manifest).expect("write manifest");
        PluginManifest::hash_bytes(manifest.as_bytes())
    }

    fn browser_voice_body() -> PluginBrowserVoiceInputBody {
        PluginBrowserVoiceInputBody {
            method: "voice.transcribe".into(),
            params: json!({}),
            session_id: "session-1".into(),
            capture_id: "capture-1".into(),
            audio: BrowserVoiceAudioUpload {
                mime_type: Some("audio/webm;codecs=opus".into()),
                duration_ms: 500,
                data_base64: "YWJj".into(),
            },
        }
    }

    #[tokio::test]
    async fn browser_voice_handler_rejects_read_only_and_invalid_metadata_before_host() {
        let mut read_only_state = crate::server::test_support::build_test_app_state(Vec::new());
        std::sync::Arc::get_mut(&mut read_only_state)
            .expect("unique test state")
            .read_only = true;
        let response = plugin_browser_voice_input(
            State(read_only_state),
            Path("dev.example.voice".into()),
            Json(browser_voice_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let mut invalid = browser_voice_body();
        invalid.method = "voice.transcribe\nforged".into();
        let response = plugin_browser_voice_input(
            State(state.clone()),
            Path("dev.example.voice".into()),
            Json(invalid),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = plugin_browser_voice_input(
            State(state),
            Path("dev.example.voice".into()),
            Json(browser_voice_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn browser_voice_route_enforces_its_local_body_limit() {
        use tower::ServiceExt;

        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        let remote: std::net::SocketAddr = "127.0.0.1:45678".parse().unwrap();
        let encoded = serde_json::to_vec(&json!({
            "method": "voice.transcribe",
            "params": {},
            "session_id": "session-1",
            "capture_id": "capture-1",
            "audio": {
                "mime_type": "audio/webm;codecs=opus",
                "duration_ms": 500,
                "data_base64": "A".repeat(1024 * 1024 + 1),
            },
        }))
        .expect("serialize voice request");
        assert!(encoded.len() > 1024 * 1024);
        assert!(encoded.len() < BROWSER_VOICE_BODY_LIMIT_BYTES);
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/plugins/dev.example.voice/browser-voice-input")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(encoded))
            .unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let response = crate::server::test_support::build_router_for_test(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response_body)
                .expect("parse response body")
                .get("error"),
            Some(&json!("no_host"))
        );

        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/plugins/dev.example.voice/browser-voice-input")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(vec![
                b'x';
                BROWSER_VOICE_BODY_LIMIT_BYTES
                    + 1
            ]))
            .unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(remote));
        let response = crate::server::test_support::build_router_for_test(state)
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn browser_voice_handler_requires_capability_then_active_action() {
        let app_dir = AppDirEnvGuard::new();
        let plain_id = "dev.example.no-microphone";
        let voice_id = "dev.example.with-microphone";
        let plain_caps = ["runtime.worker"];
        let voice_caps = ["runtime.worker", CAP_BROWSER_MICROPHONE];
        let plain_hash = write_plugin_manifest("no-microphone", plain_id, &plain_caps);
        let voice_hash = write_plugin_manifest("with-microphone", voice_id, &voice_caps);
        let mut config = Config::default();
        for (id, hash, caps) in [
            (plain_id, plain_hash, plain_caps.as_slice()),
            (voice_id, voice_hash, voice_caps.as_slice()),
        ] {
            config.plugins.insert(
                id.to_string(),
                PluginConfig {
                    grant: Some(CapabilityGrant {
                        manifest_hash: hash,
                        capabilities: caps.iter().map(|cap| cap.to_string()).collect(),
                        granted_at: chrono::Utc::now(),
                    }),
                    ..PluginConfig::default()
                },
            );
        }
        update_config(|current| *current = config).expect("save config");
        crate::plugin::reload_registry();

        let host = crate::plugin::host::PluginHost::new(app_dir._temp.path(), "default")
            .expect("create plugin host");
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        std::sync::Arc::get_mut(&mut state)
            .expect("unique test state")
            .plugin_host = Some(host);

        let response = plugin_browser_voice_input(
            State(state.clone()),
            Path(plain_id.into()),
            Json(browser_voice_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response_body)
                .expect("parse response body")
                .get("error"),
            Some(&json!("capability_required"))
        );

        let response = plugin_browser_voice_input(
            State(state),
            Path(voice_id.into()),
            Json(browser_voice_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response_body)
                .expect("parse response body")
                .get("error"),
            Some(&json!("browser_action_required"))
        );
    }

    #[test]
    #[serial_test::serial]
    fn composer_snapshot_forwarding_requires_active_composer_read_capability() {
        let _app_dir = AppDirEnvGuard::new();

        let reader_id = "dev.example.reader";
        let plain_id = "dev.example.plain";
        let reader_caps = ["runtime.worker", CAP_COMPOSER_READ];
        let plain_caps = ["runtime.worker"];
        let reader_hash = write_plugin_manifest("reader", reader_id, &reader_caps);
        let plain_hash = write_plugin_manifest("plain", plain_id, &plain_caps);

        let mut config = Config::default();
        config.plugins.insert(
            reader_id.to_string(),
            PluginConfig {
                grant: Some(CapabilityGrant {
                    manifest_hash: reader_hash,
                    capabilities: reader_caps.iter().map(|cap| cap.to_string()).collect(),
                    granted_at: chrono::Utc::now(),
                }),
                ..PluginConfig::default()
            },
        );
        config.plugins.insert(
            plain_id.to_string(),
            PluginConfig {
                grant: Some(CapabilityGrant {
                    manifest_hash: plain_hash,
                    capabilities: plain_caps.iter().map(|cap| cap.to_string()).collect(),
                    granted_at: chrono::Utc::now(),
                }),
                ..PluginConfig::default()
            },
        );
        update_config(|c| *c = config).expect("save config");
        crate::plugin::reload_registry();

        let mut reader_params = json!({
            "composer": {"text": "secret draft", "selection_start": 0, "selection_end": 6},
            "other": true,
        });
        strip_composer_snapshot_without_capability(reader_id, &mut reader_params);
        assert!(reader_params.get("composer").is_some());

        let mut plain_params = json!({
            "composer": {"text": "secret draft", "selection_start": 0, "selection_end": 6},
            "other": true,
        });
        strip_composer_snapshot_without_capability(plain_id, &mut plain_params);
        assert!(plain_params.get("composer").is_none());
        assert_eq!(plain_params.get("other"), Some(&json!(true)));
    }

    #[test]
    fn browser_voice_helpers_validate_and_merge_audio_payload() {
        assert!(is_allowed_audio_content_type("audio/webm;codecs=opus"));
        assert!(is_allowed_audio_content_type("audio/mp4"));
        assert!(!is_allowed_audio_content_type("text/plain"));
        assert!(!is_allowed_audio_content_type("audio/aac"));
        assert!(!is_allowed_audio_content_type("audio/webm;\r\nx-bad: true"));

        let mut params = json!({
            "composer": {"text": "draft", "selection_start": 0, "selection_end": 5},
        });
        merge_session_id(&mut params, Some("sess-1"));
        merge_browser_voice_audio(
            &mut params,
            "capture-1",
            "audio/webm;codecs=opus",
            3,
            1_250,
            "YWJj".to_string(),
        );

        assert_eq!(params["session_id"], json!("sess-1"));
        assert_eq!(params["composer"]["text"], json!("draft"));
        assert_eq!(params["browser"]["action"], json!("voice-input"));
        assert_eq!(params["browser"]["capture_id"], json!("capture-1"));
        assert_eq!(
            params["browser"]["audio"],
            json!({
                "mime_type": "audio/webm;codecs=opus",
                "bytes": 3,
                "duration_ms": 1250,
                "data_base64": "YWJj"
            })
        );
    }
}

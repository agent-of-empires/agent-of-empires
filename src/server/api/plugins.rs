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
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{api_error, AppState};
use crate::plugin;
use crate::plugin::install::OperationLog;
use crate::server::auth::{handler_elevated, AuthenticatedBrowserSession};
use crate::server::principal::{authorize, Capability, Principal};

const CAP_COMPOSER_READ: &str = "composer.read";
const CAP_SESSION_READ: &str = "session.read";

/// Apply mutation restrictions before resolving the authenticated principal.
async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedBrowserSession>,
    local: Option<&Principal>,
) -> Result<(), Response> {
    if state.read_only {
        return Err(super::read_only_response());
    }
    // Host-side plugin changes remain forbidden in CityHall mode.
    if let Some(resp) = super::cityhall_block(state) {
        return Err(resp);
    }
    if !handler_elevated(state, session, authorize(local, Capability::SkipElevation)).await {
        return Err(super::api_error_with_code_header(
            crate::daemon::ApiErrorCode::ElevationRequired,
            "Re-enter the passphrase to continue",
        ));
    }
    Ok(())
}

/// `GET /api/plugins`: every known plugin plus load errors.
pub async fn list_plugins(State(state): State<std::sync::Arc<AppState>>) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let registry = plugin::registry();
    Json(json!({
        "plugins": registry.all().iter().map(|p| p.view()).collect::<Vec<_>>(),
        "load_errors": registry.load_errors(),
    }))
    .into_response()
}

/// Resolve a declared plugin file (`icon_asset`, page `entry`) against its
/// install directory, refusing anything that fails `shape_ok` or lands outside
/// it. Both `dir` and the joined path are canonicalized so a symlink or `..`
/// segment cannot escape.
fn resolve_plugin_file(
    dir: &std::path::Path,
    rel: &str,
    shape_ok: fn(&str) -> bool,
) -> Option<PathBuf> {
    if !shape_ok(rel) {
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
pub async fn serve_plugin_icon(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let registry = plugin::registry();
    let Some(plugin) = registry.all().iter().find(|p| p.id() == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (Some(dir), Some(rel)) = (plugin.dir.clone(), plugin.manifest.icon_asset.clone()) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let resolved = tokio::task::spawn_blocking(move || {
        resolve_plugin_file(&dir, &rel, aoe_plugin_api::screenshot_path_ok)
    })
    .await;
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

/// Page policy from ADR 0005: only the bundle's own inline script and style run,
/// and only the dashboard may frame it. `sandbox` gives the document an opaque
/// origin even on a direct visit, not only inside the dashboard's iframe.
const PLUGIN_PAGE_CSP: &str = "sandbox allow-scripts allow-downloads; default-src 'none'; \
    script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; \
    connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

fn plugin_page_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "private, no-store"),
            (axum::http::header::CONTENT_SECURITY_POLICY, PLUGIN_PAGE_CSP),
            (axum::http::header::X_FRAME_OPTIONS, "SAMEORIGIN"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /api/plugins/{id}/pages/{page_id}`: the declared `entry` HTML of an
/// active plugin's page. Anything undeclared, missing or escaping 404s.
pub async fn serve_plugin_page(
    Path((id, page_id)): Path<(String, String)>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    // Second layer behind the CSP `sandbox`. Browsers omit `Sec-Fetch-*` outside
    // secure contexts (plain HTTP by LAN IP), so only a non-frame value is refused.
    if headers
        .get("sec-fetch-dest")
        .is_some_and(|v| v.as_bytes() != b"iframe")
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "frame_only",
            "Plugin pages load only inside the dashboard frame",
        );
    }
    // Page script can read its own URL, so a `?token=` must never reach it.
    if uri.query().is_some() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "query_not_allowed",
            "Plugin page URLs take no query string",
        );
    }
    let registry = plugin::registry();
    let Some(plugin) = registry.active().find(|p| p.id() == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (Some(dir), Some(rel)) = (
        plugin.dir.clone(),
        plugin.manifest.page_entry(&page_id).map(str::to_string),
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let resolved = tokio::task::spawn_blocking(move || {
        resolve_plugin_file(&dir, &rel, aoe_plugin_api::page_entry_path_ok)
    })
    .await;
    let Ok(Some(path)) = resolved else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => plugin_page_response(bytes),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `POST /api/plugins/{id}/pages/{page_id}/ticket`: before framing a page, the dashboard
/// lets a device-bound session hand that frame load a short-lived, page-scoped cookie, since
/// a frame navigation cannot carry the binding header. Without a login session the frame
/// authenticates as any other request and no cookie is set.
pub async fn mint_plugin_page_ticket(
    State(state): State<std::sync::Arc<AppState>>,
    Path((id, page_id)): Path<(String, String)>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
) -> Response {
    let declared = plugin::registry()
        .active()
        .any(|p| p.id() == id && p.manifest.page_entry(&page_id).is_some());
    if !declared {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut response = (
        StatusCode::NO_CONTENT,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
    )
        .into_response();
    if let Some(axum::Extension(AuthenticatedBrowserSession(session_id))) = session {
        let ticket = state
            .login_manager
            .mint_page_ticket(&session_id, &id, &page_id)
            .await;
        let cookie = crate::server::login::build_page_ticket_cookie(
            &ticket,
            &plugin::view::page_url(&id, &page_id),
            state.behind_tunnel,
        );
        response.headers_mut().append(
            axum::http::header::SET_COOKIE,
            cookie.parse().expect("manifest ids are header-safe"),
        );
    }
    response
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
pub async fn plugin_commands(State(state): State<std::sync::Arc<AppState>>) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
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
    Json(json!({ "commands": commands })).into_response()
}

/// `GET /api/plugins/ui-state`: the plugin host's aggregated UI-state snapshot
/// Includes worker slots, notifications, and active plugin pages.
pub async fn plugin_ui_state(State(state): State<std::sync::Arc<AppState>>) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let pages: Vec<_> = plugin::registry()
        .active()
        .flat_map(|plugin| plugin.pages())
        .collect();
    let empty = || json!({ "entries": [], "notifications": [] });
    let mut body = match state.plugin_host.as_ref().map(|h| h.ui_snapshot()) {
        Some(snapshot) => serde_json::to_value(snapshot).unwrap_or_else(|e| {
            // Serializing the snapshot should never fail; keep the response
            // shape stable rather than returning JSON null if it does.
            tracing::warn!(target: "serve.api", "failed to serialize plugin UI snapshot: {e}");
            empty()
        }),
        None => empty(),
    };
    body["pages"] = json!(pages);
    Json(body).into_response()
}

/// `GET /api/plugins/updates`: which installed external plugins have an update
/// available. An explicit, on-demand network check (the dashboard "Check for
/// updates" button), kept off the always-on `GET /api/plugins` list path so a
/// settings render never blocks on git/network. Allowed in read-only mode: it
/// reads remote state and mutates nothing.
pub async fn plugin_updates(State(state): State<std::sync::Arc<AppState>>) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    Json(json!({ "updates": plugin::update_check::outdated().await })).into_response()
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
pub async fn plugin_discover(
    State(state): State<std::sync::Arc<AppState>>,
    Query(query): Query<DiscoverQuery>,
) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    match plugin::discover::discover(query.q.as_deref()).await {
        Ok(results) => Json(json!({ "results": results })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, "discover_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct DetailsQuery {
    pub source: String,
}

/// `GET /api/plugins/details?source=gh:owner/repo`: the on-demand detail for one
/// plugin source (manifest fields + release tags) backing the dashboard detail
/// modal. Allowed in read-only mode; reads remote state and mutates nothing.
pub async fn plugin_details(
    State(state): State<std::sync::Arc<AppState>>,
    Query(query): Query<DetailsQuery>,
) -> Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    match plugin::discover::details(&query.source).await {
        Ok(detail) => Json(detail).into_response(),
        // `details()` only hard-errors on an invalid / unsupported `source`; a
        // GitHub fetch failure is reported in-band (manifest_error / empty
        // release tags), so a hard error here is bad client input, not an
        // upstream outage.
        Err(e) => api_error(StatusCode::BAD_REQUEST, "invalid_source", format!("{e:#}")),
    }
}

/// `GET /api/plugins/{id}/sessions`: the sessions a page bridge serves an
/// active plugin declaring `session.read`, in the rows its worker's
/// `sessions.list` returns rather than the wider `GET /api/sessions` rows.
pub async fn plugin_sessions(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if !active_plugin_declares(&id, CAP_SESSION_READ) {
        return api_error(
            StatusCode::FORBIDDEN,
            "capability_denied",
            format!("Plugin {id} does not declare {CAP_SESSION_READ}"),
        );
    }
    let _namespace = state.profile_namespace.read().await;
    // The worker's `sessions.list` scope: the host's profile, and only structured sessions in CityHall.
    let profile = crate::session::config::effective_profile(&state.served_profile());
    let instances = state.instances.read().await;
    let sessions: Vec<_> = instances
        .iter()
        .filter(|inst| inst.effective_profile() == profile)
        .filter(|inst| !state.cityhall_mode || inst.is_structured())
        .map(plugin::host_api::session_summary)
        .collect();
    Json(json!({ "sessions": sessions })).into_response()
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
    body: Result<Json<PluginActionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    // Plugin panes are hidden in CityHall (plugins are display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(host) = state.plugin_host.as_ref() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_host",
            "Plugin host is not running",
        );
    };
    // Read the UI revision now rather than the value the dashboard last polled:
    // that one is stale, so an unrelated push since would already exceed it and
    // clear the spinner early. Scoped to the firing session so another session's
    // activity cannot move it.
    let baseline_revision = host.ui_revision(&id, body.session_id.as_deref());
    // Forward the firing session so a per-session action can scope its work.
    // A worker that does not use it ignores it.
    let mut params = body.params;
    strip_composer_snapshot_without_capability(&id, &mut params);
    if let Some(sid) = &body.session_id {
        match &mut params {
            serde_json::Value::Object(map) => {
                map.insert("session_id".into(), serde_json::Value::String(sid.clone()));
            }
            serde_json::Value::Null => params = json!({ "session_id": sid }),
            _ => {}
        }
    }
    if host.notify_worker(&id, &body.method, params).await {
        (
            StatusCode::ACCEPTED,
            Json(json!({ "ok": true, "baseline_revision": baseline_revision })),
        )
            .into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "no_worker",
            format!("No running worker for plugin {id}"),
        )
    }
}

#[derive(Deserialize)]
pub struct InvokeCommandBody {
    /// The session the command is invoked from. Forwarded to the worker so a
    /// per-session command scopes its work, and validated to exist first.
    pub session_id: String,
}

/// `POST /api/plugins/commands/{fqid}/invoke`: dispatch an action-less plugin
/// command to its worker as a fixed `plugin.command.invoke` fire-and-forget
/// notification.
///
/// This is the invocation path for a command with no client `action` (the
/// GitHub plugin's `status` / `refresh`), which the palette and keybinds cannot
/// reach through `open-ui-link`. Unlike `/action` (an arbitrary caller-named
/// worker method), the command is resolved from the registry and must exist,
/// carry no client action (a client action runs on the surface, not the
/// worker), and name a live session. The fixed `plugin.command.invoke` method
/// carries `{ command, session_id }`; the worker acts on commands it knows and
/// ignores the rest.
///
/// Read-write only, no elevation, the same reasoning as `invoke_plugin_action`:
/// it mutates no host-managed state and grants no capability.
pub async fn invoke_plugin_command(
    State(state): State<std::sync::Arc<AppState>>,
    Path(fqid): Path<String>,
    Json(body): Json<InvokeCommandBody>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    // Plugin commands are not surfaced in CityHall (plugins are display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    // Plugin ids contain dots, so resolve fqid against the registry rather
    // than string-splitting it.
    let mut resolved: Option<(String, bool)> = None;
    for p in plugin::registry().active() {
        let plugin_id = p.id().to_string();
        for c in &p.manifest.commands {
            if fqid == format!("plugin.{plugin_id}.{}", c.id) {
                resolved = Some((plugin_id.clone(), c.action.is_some()));
            }
        }
    }
    let Some((plugin_id, has_action)) = resolved else {
        return api_error(
            StatusCode::NOT_FOUND,
            "unknown_command",
            format!("No active plugin command {fqid}"),
        );
    };
    if has_action {
        return api_error(
            StatusCode::BAD_REQUEST,
            "client_action_command",
            format!(
                "{fqid} carries a client action; it is executed on the surface, not the worker"
            ),
        );
    }
    if !state
        .instances
        .read()
        .await
        .iter()
        .any(|i| i.id == body.session_id)
    {
        return api_error(
            StatusCode::NOT_FOUND,
            "unknown_session",
            format!("No session {}", body.session_id),
        );
    }
    let Some(host) = state.plugin_host.as_ref() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_host",
            "Plugin host is not running",
        );
    };
    let params = json!({ "command": fqid, "session_id": body.session_id });
    if host
        .notify_worker(&plugin_id, "plugin.command.invoke", params)
        .await
    {
        (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "no_worker",
            format!("No running worker for plugin {plugin_id}"),
        )
    }
}

fn active_plugin_declares(plugin_id: &str, capability: &str) -> bool {
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

fn strip_composer_snapshot_without_capability(plugin_id: &str, params: &mut serde_json::Value) {
    if active_plugin_declares(plugin_id, CAP_COMPOSER_READ) {
        return;
    }
    if let serde_json::Value::Object(map) = params {
        map.remove("composer");
    }
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }
    match plugin::install::preview_update(&id).await {
        Ok(preview) => Json(preview).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, "preview_failed", format!("{e:#}")),
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(id): Path<String>,
    Json(body): Json<ApplyUpdateBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let plugin_id = id.clone();
    let fingerprint = body.expected_fingerprint;
    let host = state.plugin_host.clone();
    start_job(state, PluginJobKind::Update, id, move |log| async move {
        plugin::install::apply_update(&plugin_id, fingerprint, &log).await?;
        if let Some(host) = host {
            host.restart_worker(&plugin_id, &plugin::registry()).await;
        }
        Ok(())
    })
    .await
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(id): Path<String>,
    Json(body): Json<DismissUpdateBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let plugin = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        plugin::install::dismiss_update(&id, &body.fingerprint)
    })
    .await;
    match result {
        Ok(Ok(())) => {
            tracing::debug!(target: "plugin.host", %plugin, "plugin update dismissed");
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

/// `POST /api/plugins/{id}/enabled`
pub async fn set_plugin_enabled(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabledBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let plugin = id.clone();
    let enabled = body.enabled;
    let result =
        tokio::task::spawn_blocking(move || plugin::install::set_enabled(&id, body.enabled)).await;
    match result {
        Ok(Ok(())) => {
            // set_enabled reloaded the on-disk registry; reconcile the live
            // host so enabling launches the worker and disabling tears it down
            // without a daemon restart. reconcile is async, so it runs after
            // the sync spawn_blocking returns, never inside it.
            if let Some(host) = state.plugin_host.clone() {
                host.reconcile(&crate::plugin::registry()).await;
            }
            list_plugins(State(state)).await
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "plugin.host", %plugin, enabled, error = %format!("{e:#}"), "plugin toggle failed");
            api_error(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}"))
        }
        Err(e) => {
            tracing::warn!(target: "plugin.host", %plugin, enabled, error = %e, "plugin toggle task failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
        }
    }
}

/// `POST /api/plugins/{id}/worker/restart`: reload plugins from disk and
/// replace this plugin's worker, so an update applied outside the daemon (CLI,
/// TUI) runs the new build without a daemon restart.
pub async fn restart_plugin_worker(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let registry = match tokio::task::spawn_blocking(plugin::reload_registry).await {
        Ok(registry) => registry,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    tracing::info!(target: "plugin.host", plugin = %id, "worker restart requested");
    if let Some(host) = state.plugin_host.clone() {
        host.restart_worker(&id, &registry).await;
    }
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// Plugin lifecycle jobs: install, update, and uninstall.

use std::sync::atomic::{AtomicBool, Ordering};

/// A host-side plugin lifecycle operation the dashboard started and tails. The
/// daemon owns the work; the browser polls `GET /api/plugins/jobs/{id}` for
/// status plus the live log tail.
#[derive(Clone, Copy, Debug, Serialize)]
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
    #[serde(skip)]
    started: std::time::Instant,
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
            started: std::time::Instant::now(),
        };
        self.jobs.lock().unwrap().insert(id.clone(), job);
        Some((id, log_path))
    }

    /// Mark a job done and release the single-active guard.
    fn finish(&self, id: &str, result: anyhow::Result<()>) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.finished_at = Some(chrono::Utc::now().timestamp());
            let duration_ms = job.started.elapsed().as_millis();
            job.status = match result {
                Ok(()) => {
                    tracing::info!(target: "plugin.install", job_id = %id, kind = ?job.kind, target = %job.target, duration_ms, "plugin job finished");
                    PluginJobStatus::Succeeded
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    tracing::warn!(target: "plugin.install", job_id = %id, kind = ?job.kind, target = %job.target, duration_ms, %error, "plugin job finished");
                    PluginJobStatus::Failed { error }
                }
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

/// Admit one lifecycle job; return its id while its work remains tracked.
async fn start_job<F, Fut>(
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
        return api_error(
            StatusCode::CONFLICT,
            "plugin_job_active",
            "Another plugin operation is already running",
        );
    };
    let jobs = state.plugin_jobs.clone();
    let id = job_id.clone();
    state.runtime.work.spawn("server.plugin_job", async move {
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
        return super::read_only_response();
    }
    // The marketplace / install flow is hidden and closed in CityHall.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    match plugin::install::preview_install(&body.source).await {
        Ok(consent) => Json(consent).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, "preview_failed", format!("{e:#}")),
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
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Json(body): Json<StartInstallBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
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
    .await
}

/// `POST /api/plugins/{id}/uninstall`: start a host-side uninstall job. Removes
/// the plugin's tree, config entry, and lockfile entry. Read-write + elevation;
/// returns a `job_id` to poll; `409` if another lifecycle job is running.
pub async fn start_plugin_uninstall(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
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
    .await
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Some(job) = state.plugin_jobs.get(&job_id) else {
        return api_error(
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
        Ok(Err(e)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "log_read_failed",
            format!("{e}"),
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
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
    fn resolve_plugin_file_confines_to_the_install_dir() {
        use aoe_plugin_api::{page_entry_path_ok, screenshot_path_ok};
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugin");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        for file in [
            "plugin/icon.png",
            "plugin/icon.svg",
            "plugin/dist/page.html",
            "secret.png",
            "secret.html",
        ] {
            std::fs::write(root.path().join(file), b"x").unwrap();
        }
        std::os::unix::fs::symlink(root.path().join("secret.png"), dir.join("link.png")).unwrap();
        std::os::unix::fs::symlink(root.path().join("secret.html"), dir.join("link.html")).unwrap();
        let canon = dir.canonicalize().unwrap();
        assert_eq!(
            resolve_plugin_file(&dir, "icon.png", screenshot_path_ok),
            Some(canon.join("icon.png"))
        );
        assert_eq!(
            resolve_plugin_file(&dir, "dist/page.html", page_entry_path_ok),
            Some(canon.join("dist/page.html"))
        );
        // Each bad path trips one guard alone: shape, symlink escape or absence.
        let cases: [(&str, fn(&str) -> bool); 9] = [
            ("../secret.png", screenshot_path_ok),
            ("/etc/passwd.png", screenshot_path_ok),
            ("icon.svg", screenshot_path_ok),
            ("link.png", screenshot_path_ok),
            ("missing.png", screenshot_path_ok),
            ("../secret.html", page_entry_path_ok),
            ("link.html", page_entry_path_ok),
            ("icon.png", page_entry_path_ok),
            ("missing.html", page_entry_path_ok),
        ];
        for (bad, shape_ok) in cases {
            assert!(
                resolve_plugin_file(&dir, bad, shape_ok).is_none(),
                "{bad:?} should not resolve"
            );
        }
    }

    #[tokio::test]
    async fn only_the_plugin_page_relaxes_the_global_frame_policy() {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route(
                "/page",
                axum::routing::get(|| async { plugin_page_response(b"<p>hi</p>".to_vec()) }),
            )
            .route("/other", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(
                crate::server::access::security_headers,
            ));
        let cases = [
            (
                "/page",
                "sandbox allow-scripts allow-downloads; default-src 'none'; script-src 'unsafe-inline'; \
                 style-src 'unsafe-inline'; img-src data: blob:; connect-src 'none'; \
                 form-action 'none'; base-uri 'none'; frame-ancestors 'self'",
                "SAMEORIGIN",
            ),
            ("/other", crate::server::access::CSP, "DENY"),
        ];
        for (path, csp, frame_options) in cases {
            let req = axum::http::Request::get(path)
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let header = |name| resp.headers().get(name).unwrap().to_str().unwrap();
            assert_eq!(header("content-security-policy"), csp, "{path}");
            assert_eq!(header("x-frame-options"), frame_options, "{path}");
        }
        let req = axum::http::Request::get("/page")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let header = |name| resp.headers().get(name).unwrap().to_str().unwrap();
        assert_eq!(header("content-type"), "text/html; charset=utf-8");
        assert_eq!(header("cache-control"), "private, no-store");
    }

    #[tokio::test]
    async fn plugin_page_refuses_query_strings_and_non_frame_loads() {
        use tower::ServiceExt;
        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("secret-token".to_string()),
        );
        let app = crate::server::test_support::build_router_for_test(state);
        let cookie = Some("aoe_token=secret-token");
        let frame = Some("iframe");
        // NOT_FOUND means every guard passed and only the plugin is unknown.
        let cases = [
            ("?token=secret-token", None, frame, StatusCode::BAD_REQUEST),
            (
                "?token=secret-token",
                cookie,
                frame,
                StatusCode::BAD_REQUEST,
            ),
            ("", cookie, frame, StatusCode::NOT_FOUND),
            ("", cookie, Some("document"), StatusCode::FORBIDDEN),
            // Plain HTTP by LAN IP: no `Sec-Fetch-*` at all.
            ("", cookie, None, StatusCode::NOT_FOUND),
            ("?token=secret-token", cookie, None, StatusCode::BAD_REQUEST),
            ("?token=wrong", None, frame, StatusCode::UNAUTHORIZED),
        ];
        for (query, cookie, dest, want) in cases {
            let mut req =
                axum::http::Request::get(format!("/api/plugins/acme.absent/pages/board{query}"))
                    .header("host", "127.0.0.1");
            if let Some(c) = cookie {
                req = req.header("cookie", c);
            }
            if let Some(d) = dest {
                req = req.header("sec-fetch-dest", d);
            }
            let mut req = req.body(axum::body::Body::empty()).unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [203, 0, 113, 7],
                    5555,
                ))));
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                want,
                "{query} cookie={cookie:?} dest={dest:?}"
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

    struct AppDirEnvGuard {
        // Field drop order is load-bearing: `_env` restores HOME / XDG /
        // USERPROFILE and releases the shared env lock first, then `_reload`
        // reloads the registry against the restored dirs, then `_temp` deletes
        // the tempdir (#2864, #2600).
        _env: crate::session::test_support::EnvGuard,
        _reload: crate::plugin::ReloadRegistryOnDrop,
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
                _reload: crate::plugin::ReloadRegistryOnDrop,
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

    /// Install each `(id, granted)` plugin declaring `pages`, each served as `<p>{id}</p>`.
    fn install_page_plugins(plugins: &[(&str, bool)], pages: &[&str]) {
        let mut config = Config::default();
        for &(id, granted) in plugins {
            let mut manifest =
                format!("id = \"{id}\"\nname = \"Board\"\nversion = \"1.0.0\"\napi_version = 16\n");
            let dir = crate::plugin::plugins_dir().unwrap().join(id);
            std::fs::create_dir_all(dir.join("dist")).unwrap();
            for page in pages {
                manifest.push_str(&format!(
                    "\n[[ui]]\nslot = \"page\"\nid = \"{page}\"\ntitle = \"Board\"\nentry = \"dist/{page}.html\"\n"
                ));
                std::fs::write(
                    dir.join(format!("dist/{page}.html")),
                    format!("<p>{id}</p>"),
                )
                .unwrap();
            }
            std::fs::write(dir.join("aoe-plugin.toml"), &manifest).unwrap();
            if granted {
                config.plugins.insert(
                    id.to_string(),
                    PluginConfig {
                        grant: Some(CapabilityGrant {
                            manifest_hash: PluginManifest::hash_bytes(manifest.as_bytes()),
                            capabilities: Vec::new(),
                            granted_at: chrono::Utc::now(),
                        }),
                        ..PluginConfig::default()
                    },
                );
            }
        }
        update_config(|c| *c = config).expect("save config");
        crate::plugin::reload_registry();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_device_bound_frame_loads_its_page_only_through_a_page_ticket() {
        use base64::Engine;
        use tower::ServiceExt;
        let _app_dir = AppDirEnvGuard::new();
        install_page_plugins(&[("acme.board", true)], &["board", "notes"]);

        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("secret-token".to_string()),
            |state| {
                state.login_manager =
                    std::sync::Arc::new(crate::server::login::LoginManager::new(Some("pw")))
            },
        );
        let login = std::sync::Arc::clone(&state.login_manager);
        let binding = [7u8; 32];
        let session = login.create_session(&binding, "203.0.113.7", "t").await;
        let binding = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(binding);
        let app = crate::server::test_support::build_router_for_test(state);
        let send =
            |method: &str, path: &str, cookie: String, bound: bool| {
                let mut req = axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "127.0.0.1")
                    .header("cookie", cookie)
                    .header("sec-fetch-dest", "iframe");
                if bound {
                    req = req.header("x-aoe-device-binding", &binding);
                }
                let mut req = req.body(axum::body::Body::empty()).unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(
                    std::net::SocketAddr::from(([203, 0, 113, 7], 5555)),
                ));
                app.clone().oneshot(req)
            };
        let board = "/api/plugins/acme.board/pages/board";
        let base = format!("aoe_token=secret-token; aoe_session={session}");

        let resp = send("GET", board, base.clone(), false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "no ticket");
        let resp = send("POST", &format!("{board}/ticket"), base.clone(), false)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "unbound mint");
        let resp = send(
            "POST",
            "/api/plugins/acme.board/pages/absent/ticket",
            base.clone(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "undeclared page");

        let resp = send("POST", &format!("{board}/ticket"), base.clone(), true)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let set_cookie = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .find(|c| c.starts_with("aoe_page_ticket="))
            .expect("ticket cookie");
        for attr in [
            "HttpOnly",
            "SameSite=Strict",
            &format!("Path={board};"),
            "Max-Age=60",
        ] {
            assert!(set_cookie.contains(attr), "{set_cookie} lacks {attr}");
        }
        let ticket = set_cookie.split(';').next().unwrap().to_string();
        let with_ticket = format!("{base}; {ticket}");

        let resp = send("GET", board, with_ticket.clone(), false)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "ticketed frame load");
        for path in [
            "/api/plugins/acme.board/pages/notes",
            "/api/plugins/ui-state",
        ] {
            let resp = send("GET", path, with_ticket.clone(), false).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
        }

        login.invalidate_session(&session).await;
        let resp = send("GET", board, with_ticket, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "after logout");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_active_plugins_page_is_delivered_through_the_real_route() {
        use tower::ServiceExt;
        let _app_dir = AppDirEnvGuard::new();
        // `acme.idle` is installed but never granted, so it is inactive.
        install_page_plugins(&[("acme.board", true), ("acme.idle", false)], &["board"]);

        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("secret-token".to_string()),
        );
        let app = crate::server::test_support::build_router_for_test(state);
        let get =
            |path: &str| {
                let mut req = axum::http::Request::get(path)
                    .header("host", "127.0.0.1")
                    .header("cookie", "aoe_token=secret-token")
                    .header("sec-fetch-dest", "iframe")
                    .body(axum::body::Body::empty())
                    .unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(
                    std::net::SocketAddr::from(([203, 0, 113, 7], 5555)),
                ));
                app.clone().oneshot(req)
            };

        let resp = get("/api/plugins/acme.board/pages/board").await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let header = |name| {
            resp.headers()
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(header("content-security-policy"), PLUGIN_PAGE_CSP);
        assert_eq!(header("x-frame-options"), "SAMEORIGIN");
        assert_eq!(header("content-type"), "text/html; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"<p>acme.board</p>");

        let resp = get("/api/plugins/acme.idle/pages/board").await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get("/api/plugins/ui-state").await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            state["pages"],
            json!([{
                "plugin_id": "acme.board",
                "id": "board",
                "title": "Board",
                "icon": null,
                "url": "/api/plugins/acme.board/pages/board",
                "capabilities": [],
            }])
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

    #[tokio::test]
    #[serial_test::serial]
    async fn plugin_sessions_serve_host_profile_worker_rows_only_with_session_read() {
        use tower::ServiceExt;
        let _app_dir = AppDirEnvGuard::new();
        let reader_caps = ["runtime.worker", CAP_SESSION_READ];
        let plain_caps = ["runtime.worker"];
        let mut config = Config::default();
        for (dir, id, caps) in [
            ("reader", "dev.example.reader", &reader_caps[..]),
            ("plain", "dev.example.plain", &plain_caps[..]),
        ] {
            let hash = write_plugin_manifest(dir, id, caps);
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
        update_config(|c| *c = config).expect("save config");
        crate::plugin::reload_registry();

        let mut inst = crate::session::Instance::new("alpha", "/tmp/alpha");
        inst.source_profile = "test".into();
        let expected = json!([crate::plugin::host_api::session_summary(&inst)]);
        let mut other = crate::session::Instance::new("beta", "/tmp/beta");
        other.source_profile = "other".into();
        let state = crate::server::test_support::build_test_app_state(vec![inst, other]);
        let app = crate::server::test_support::build_router_for_test(state);
        let get =
            |path: &str| {
                let mut req = axum::http::Request::get(path)
                    .header("host", "127.0.0.1")
                    .body(axum::body::Body::empty())
                    .unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 5555)),
                ));
                app.clone().oneshot(req)
            };

        let resp = get("/api/plugins/dev.example.reader/sessions")
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["sessions"], expected);

        for id in ["dev.example.plain", "dev.example.absent"] {
            let resp = get(&format!("/api/plugins/{id}/sessions")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{id}");
        }
    }
}

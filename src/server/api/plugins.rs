//! Plugin management REST API: list plugins and enable/disable them, the web
//! twin of `aoe plugin`. The enable/disable toggle runs on the host, so it
//! requires read-write mode and an elevated session when login is enabled.

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
use crate::server::auth::{handler_elevated, AuthenticatedSession, LoopbackTrusted};

const CAP_COMPOSER_READ: &str = "composer.read";

/// Resolve the read-only and elevation gates shared by every mutation.
/// Elevation goes through `handler_elevated`, so a loopback-trusted caller
/// passes without a session: the bypass paths never insert
/// `AuthenticatedSession`, and treating that as not-elevated made these
/// mutations unreachable from localhost (#2610).
async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
    loopback_trusted: bool,
) -> Result<(), Response> {
    if state.read_only {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode",
        ));
    }
    // CityHall renders the Plugins tab read-only: install / uninstall / enable /
    // update all mutate host state, and elevation is no barrier for a
    // locked-down user holding the passphrase or running `--auth=none` (#7).
    if let Some(resp) = super::cityhall_block(state) {
        return Err(resp);
    }
    if !handler_elevated(state, session, loopback_trusted).await {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue",
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

/// Resolve a plugin's declared `icon_asset` against its install directory,
/// refusing anything outside it. Both `dir` and the joined path are
/// canonicalized so a symlink or `..` segment cannot escape. Takes the
/// already-loaded `dir`/`rel` rather than touching the registry, so it stays
/// testable without a running plugin host.
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

/// `GET /api/plugins/{id}/icon`: stream an installed plugin's `icon_asset` from
/// its install directory. Like `serve_sound_file`, the manifest path is
/// re-validated and re-joined rather than trusted from a cached URL. A builtin
/// or a plugin with no `icon_asset` 404s.
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
/// keymap: the namespaced `fqid`, its keybind chords, and its optional
/// client-executed `action`.
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

/// `GET /api/plugins/commands`: active plugins' contributed commands with their
/// chords and client actions. Reads manifests, not workers, so it is safe in
/// read-only mode.
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
/// plus the notification ring. Empty when no host is running.
pub async fn plugin_ui_state(
    State(state): State<std::sync::Arc<AppState>>,
) -> Json<serde_json::Value> {
    let empty = || json!({ "entries": [], "notifications": [] });
    match state.plugin_host.as_ref().map(|h| h.ui_snapshot()) {
        Some(snapshot) => Json(serde_json::to_value(snapshot).unwrap_or_else(|e| {
            // Serializing the snapshot should never fail; keep the response
            // shape stable rather than returning JSON null if it does.
            tracing::warn!(target: "serve.api", "failed to serialize plugin UI snapshot: {e}");
            empty()
        })),
        None => Json(empty()),
    }
}

/// `GET /api/plugins/updates`: which installed external plugins have an update.
/// An explicit, on-demand network check kept off the always-on
/// `GET /api/plugins` path so a settings render never blocks on git. Allowed in
/// read-only mode: it mutates nothing.
pub async fn plugin_updates() -> Json<serde_json::Value> {
    Json(json!({ "updates": plugin::update_check::outdated().await }))
}

#[derive(Deserialize)]
pub struct DiscoverQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /api/plugins/discover?q=`: search the `aoe-plugin` GitHub topic.
/// Browse-only, since capability approval needs a terminal, so each result
/// carries an `install_command` to copy. A GitHub failure (notably the
/// unauthenticated rate limit) returns its message rather than a generic 500.
pub async fn plugin_discover(Query(query): Query<DiscoverQuery>) -> Response {
    match plugin::discover::discover(query.q.as_deref()).await {
        Ok(results) => Json(json!({ "results": results })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, "discover_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct DetailsQuery {
    pub source: String,
}

/// `GET /api/plugins/details?source=gh:owner/repo`: manifest fields plus
/// release tags for one plugin source, backing the detail modal. Allowed in
/// read-only mode.
pub async fn plugin_details(Query(query): Query<DetailsQuery>) -> Response {
    match plugin::discover::details(&query.source).await {
        Ok(detail) => Json(detail).into_response(),
        // `details()` hard-errors only on an invalid source; a GitHub fetch
        // failure is reported in-band, so a hard error is bad client input.
        Err(e) => api_error(StatusCode::BAD_REQUEST, "invalid_source", format!("{e:#}")),
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
    /// waits only for it. Merged into `params.session_id` before forwarding.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `POST /api/plugins/{id}/action`: forward a dashboard UI action to the
/// plugin's worker as a fire-and-forget JSON-RPC notification. The worker is the
/// trust boundary, acting only on methods it implements, so this never waits for
/// a result.
///
/// Read-write only, not elevation: a UI action mutates no host-managed state and
/// grants no capability, so a routine `github.refresh` should not prompt for the
/// passphrase.
pub async fn invoke_plugin_action(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<PluginActionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return api_error(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode",
        );
    }
    // Plugin panes are hidden in CityHall (plugins are display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    // Extracted after the guards so a read-only or CityHall server answers with
    // its own status rather than a body-shape 400 (#1229).
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
/// command to its worker as a fixed `plugin.command.invoke` notification.
///
/// This is the path for a command with no client `action`, which the palette and
/// keybinds cannot reach through `open-ui-link`. Unlike `/action`, the command
/// is resolved from the registry and must exist, carry no client action, and
/// name a live session.
///
/// Read-write only, no elevation, as for `invoke_plugin_action`.
pub async fn invoke_plugin_command(
    State(state): State<std::sync::Arc<AppState>>,
    Path(fqid): Path<String>,
    Json(body): Json<InvokeCommandBody>,
) -> Response {
    if state.read_only {
        return api_error(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode",
        );
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

fn strip_composer_snapshot_without_capability(plugin_id: &str, params: &mut serde_json::Value) {
    let can_read = plugin::registry()
        .get(plugin_id)
        .filter(|p| p.active())
        .is_some_and(|p| {
            p.manifest
                .capabilities
                .iter()
                .any(|cap| cap.as_str() == CAP_COMPOSER_READ)
        });
    if can_read {
        return;
    }
    if let serde_json::Value::Object(map) = params {
        map.remove("composer");
    }
}

/// `GET /api/plugins/{id}/update/preview`: classify an available update
/// (no_update / safe_update / consent_required) and return the disclosure the
/// dashboard and TUI render. Read-write only, NOT elevation: it powers the
/// approval UI, so a non-elevated session must be able to fetch the capability
/// diff before deciding. Network failures surface as a 502.
pub async fn plugin_update_preview(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.read_only {
        return api_error(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode",
        );
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

/// `POST /api/plugins/{id}/update/apply`: apply an update the user approved.
/// A privileged host mutation (it can expand the capability set and run build
/// steps), so it needs read-write mode AND elevation. Runs as a host-side job
/// and returns a `job_id` to poll. A fingerprint mismatch surfaces as a failed
/// job, which the UI recovers from by re-previewing.
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
    let host = state.plugin_host.clone();
    start_job(state, PluginJobKind::Update, id, move |log| async move {
        plugin::install::apply_update(&plugin_id, fingerprint, &log).await?;
        if let Some(host) = host {
            host.restart_worker(&plugin_id, &plugin::registry()).await;
        }
        Ok(())
    })
}

#[derive(Deserialize)]
pub struct DismissUpdateBody {
    /// The fingerprint of the update the user declined, from the preview.
    pub fingerprint: String,
}

/// `POST /api/plugins/{id}/update/dismiss`: record that the user declined an
/// update, so nagging stops until the next version. Mutates host config and
/// suppresses a security signal, so it is gated like apply.
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
            // set_enabled reloaded the on-disk registry; reconcile the live
            // host so enabling launches the worker and disabling tears it down
            // without a daemon restart. reconcile is async, so it runs after
            // the sync spawn_blocking returns, never inside it.
            if let Some(host) = state.plugin_host.clone() {
                host.reconcile(&crate::plugin::registry()).await;
            }
            list_plugins().await.into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /api/plugins/{id}/worker/restart`: reload plugins from disk and
/// replace this plugin's worker, so an update applied outside the daemon (CLI,
/// TUI) runs the new build without a daemon restart.
pub async fn restart_plugin_worker(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<LoopbackTrusted>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.is_some()).await {
        return resp;
    }
    let registry = match tokio::task::spawn_blocking(plugin::reload_registry).await {
        Ok(registry) => registry,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    if let Some(host) = state.plugin_host.clone() {
        host.restart_worker(&id, &registry).await;
    }
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// Plugin lifecycle jobs: install, update, and uninstall.

use std::sync::atomic::{AtomicBool, Ordering};

/// A host-side plugin lifecycle operation the dashboard started and tails. The
/// daemon owns the work; the browser polls `GET /api/plugins/jobs/{id}`.
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
/// starts. A dashboard polls for seconds to minutes, so an hour bounds the
/// in-memory map and the on-disk logs with a wide margin.
const FINISHED_JOB_TTL_SECS: i64 = 3600;

/// In-memory registry of plugin lifecycle jobs. Dies with the daemon: a job
/// running at shutdown is gone, but its on-disk log survives so a tail after a
/// restart still shows what happened.
// ponytail: a persisted job table would need process supervision and
// orphaned-build recovery to mean anything.
pub struct PluginJobRegistry {
    jobs: Mutex<HashMap<String, PluginJob>>,
    /// At most one lifecycle mutation runs at a time: config and lockfile
    /// writes and in-place tree mutations are not concurrency-safe, so a second
    /// start is rejected with 409 rather than queued.
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

/// Begin a lifecycle job, spawn its work, and return `202 { job_id }`, or `409`
/// when another lifecycle mutation is running. The work runs in a detached task
/// whose output lands in the job log file the dashboard tails.
// ponytail: install/update run their synchronous build inside this async task,
// parking one runtime worker; the single-active guard caps that at one.
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
        return api_error(
            StatusCode::CONFLICT,
            "plugin_job_active",
            "Another plugin operation is already running",
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
/// return the disclosure the dashboard renders before approval. Read-write only,
/// NOT elevation: it mutates nothing and powers the approval UI.
pub async fn preview_plugin_install(
    State(state): State<std::sync::Arc<AppState>>,
    Json(body): Json<InstallPreviewBody>,
) -> Response {
    if state.read_only {
        return api_error(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode",
        );
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

/// `POST /api/plugins/install`: start a host-side install job for an approved
/// `gh:` source. Read-write plus elevation, like update apply. Returns a
/// `job_id` to poll; `409` if another lifecycle job is running.
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

/// `POST /api/plugins/{id}/uninstall`: start a host-side uninstall job,
/// removing the plugin's tree, config entry and lockfile entry. Read-write plus
/// elevation; returns a `job_id` to poll, or `409` if one is running.
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
/// tail of its host-side log, polled by the dashboard progress modal.
pub async fn plugin_job_status(
    State(state): State<std::sync::Arc<AppState>>,
    Path(job_id): Path<String>,
    Query(q): Query<JobLogQuery>,
) -> Response {
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
    fn resolve_plugin_icon_path_confines_to_the_install_dir() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugin");
        std::fs::create_dir(&dir).unwrap();
        for file in ["plugin/icon.png", "plugin/icon.svg", "secret.png"] {
            std::fs::write(root.path().join(file), b"x").unwrap();
        }
        std::os::unix::fs::symlink(root.path().join("secret.png"), dir.join("link.png")).unwrap();
        assert_eq!(
            resolve_plugin_icon_path(&dir, "icon.png"),
            Some(dir.canonicalize().unwrap().join("icon.png"))
        );
        // `icon.svg` exists inside but fails the shape check; `link.png` passes
        // it and escapes through a symlink, so each guard is pinned alone.
        for bad in [
            "../secret.png",
            "/etc/passwd.png",
            "icon.svg",
            "link.png",
            "missing.png",
        ] {
            assert!(
                resolve_plugin_icon_path(&dir, bad).is_none(),
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
}

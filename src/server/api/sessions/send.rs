//! Send-message, paste-image, and read-output endpoints.

use super::*;

use crate::server::session_service::SessionService;
use crate::session::SessionStore;

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    /// Whether to auto-revive a dead/stopped session before sending. Defaults
    /// to `true`; set to `false` for fail-loud behavior (parity with the
    /// `--no-revive` CLI flag).
    #[serde(default = "default_revive")]
    pub revive: bool,
}

fn default_revive() -> bool {
    true
}

/// Terminal delivery failures shared by HTTP and plugin callers.
pub(crate) enum SendKeysError {
    NotFound,
    NotRunning,
    Transient(Status),
    StructuredView,
    Blocked(crate::session::StartBlocked),
    Gone,
    Tmux(anyhow::Error),
    Preparation {
        response: Box<axum::response::Response>,
        detail: serde_json::Value,
    },
    Internal,
}

pub(crate) struct TerminalSendError {
    pub error: SendKeysError,
    committed_cursor: Option<Box<(axum::http::HeaderValue, axum::http::HeaderValue)>>,
}

/// Revive through the native lifecycle command, then send under immutable pane ownership.
pub(crate) async fn send_terminal_message(
    service: &SessionService,
    id: &str,
    message: String,
    revive: bool,
) -> Result<bool, TerminalSendError> {
    let mut committed_cursor = None;
    send_terminal_message_inner(service, id, message, revive, &mut committed_cursor)
        .await
        .map_err(|error| TerminalSendError {
            error,
            committed_cursor: committed_cursor.map(Box::new),
        })
}

async fn send_terminal_message_inner(
    service: &SessionService,
    id: &str,
    message: String,
    revive: bool,
    committed_cursor: &mut Option<(axum::http::HeaderValue, axum::http::HeaderValue)>,
) -> Result<bool, SendKeysError> {
    let state = service.native_state().map_err(SendKeysError::Tmux)?;
    if state.read_only {
        return Err(SendKeysError::Preparation {
            response: Box::new(crate::server::api::read_only_response()),
            detail: serde_json::json!({"error": "read_only"}),
        });
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return Err(SendKeysError::Preparation {
            response: Box::new(response),
            detail: serde_json::json!({"error": "cityhall_mode"}),
        });
    }
    let before = service
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or(SendKeysError::NotFound)?;
    before.ensure_startable().map_err(SendKeysError::Blocked)?;
    if before.is_structured() {
        return Err(SendKeysError::StructuredView);
    }
    if matches!(before.status, Status::Creating | Status::Deleting) {
        return Err(SendKeysError::Transient(before.status));
    }
    let original_source =
        crate::session::Storage::open_reference(&before.source_profile).map_err(|error| {
            terminal_send_failure(
                &state,
                error.context(crate::session::NativeInputError::Ownership),
            )
        })?;
    let initial_identity = crate::daemon::NativePreparationIdentity::from_instance(
        &before,
        original_source
            .bound_profile_identity()
            .map_err(|error| terminal_send_failure(&state, error))?,
    );
    #[cfg(test)]
    super::tests::terminal_custody::pause(
        id,
        super::tests::terminal_custody::Boundary::SendInitialSnapshot,
    )
    .await;
    let initial_namespace = state.profile_namespace.read().await;
    let initial_lock = state.instance_lock(id).await;
    let initial_guard = initial_lock.lock().await;
    let worker_state = state.clone();
    let (native, initial_identity, existing_target) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut before = before;
            anyhow::ensure!(
                initial_identity.matches_source(&original_source),
                crate::session::LifecycleReservationError::Superseded
            );
            let native = crate::server::session_store::NativeSessionStore::open(
                worker_state,
                &before.source_profile,
                None,
            )?;
            anyhow::ensure!(
                initial_identity.matches_source(native.storage()),
                crate::session::LifecycleReservationError::Superseded
            );
            let _ownership = before.acquire_auxiliary_locks_in(&native)?;
            anyhow::ensure!(
                !before.is_structured(),
                crate::session::NativeInputError::Ownership
            );
            anyhow::ensure!(
                initial_identity.matches(&before),
                crate::session::LifecycleReservationError::Superseded
            );
            let existing = super::ensure::physical_agent_target(&before)
                .map_err(|error| error.context(crate::session::NativeInputError::Ownership))?
                .map(|(tmux_session, physical)| crate::daemon::TerminalTarget {
                    tmux_session,
                    physical: Some(physical),
                    lifecycle_generation: before.lifecycle_generation,
                    profile: before.source_profile.clone(),
                    status: crate::daemon::TerminalTargetStatus::Alive,
                });
            Ok((native, initial_identity, existing))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .map_err(|error| terminal_send_failure(&state, error))?;
    drop(initial_guard);
    drop(initial_namespace);
    let target = if revive {
        let response = super::lifecycle::prepare_agent_session(
            state.clone(),
            id.to_owned(),
            Ok(Some(Json(crate::daemon::StartSessionBody {
                size: None,
                expected: Some(initial_identity.clone()),
            }))),
            super::lifecycle::AgentPreparation::Ensure,
            None,
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .map_err(|_| SendKeysError::Internal)?;
        if !parts.status.is_success() {
            let detail = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            return Err(SendKeysError::Preparation {
                response: Box::new(axum::response::Response::from_parts(
                    parts,
                    axum::body::Body::from(bytes),
                )),
                detail,
            });
        }
        let target: crate::daemon::TerminalTarget =
            serde_json::from_slice(&bytes).map_err(|_| SendKeysError::Internal)?;
        if target.lifecycle_generation != initial_identity.lifecycle_generation {
            *committed_cursor = parts
                .headers
                .get(crate::daemon::RUNTIME_EPOCH_HEADER)
                .cloned()
                .zip(
                    parts
                        .headers
                        .get(crate::daemon::RUNTIME_REVISION_HEADER)
                        .cloned(),
                );
        }
        target
    } else {
        existing_target.ok_or(SendKeysError::NotRunning)?
    };
    let revived = target.lifecycle_generation != initial_identity.lifecycle_generation;
    #[cfg(test)]
    super::tests::terminal_custody::pause(
        id,
        super::tests::terminal_custody::Boundary::SendAdmitted,
    )
    .await;
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return Err(SendKeysError::Tmux(
            crate::session::NativeStoreUnavailable.into(),
        ));
    }
    let _submission = service
        .prompt_submission_for_session(id)
        .await
        .ok_or(SendKeysError::NotFound)?;
    let lock = service.instance_lock(id).await;
    let guard = lock.lock().await;
    let mut instance = service
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or(SendKeysError::NotFound)?;
    let expected = crate::daemon::NativePreparationIdentity {
        created_at: initial_identity.created_at,
        lifecycle_generation: target.lifecycle_generation,
        profile: initial_identity.profile,
        profile_identity: initial_identity.profile_identity,
        selection: initial_identity.selection,
    };
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        anyhow::ensure!(
            target.profile == expected.profile
                && expected.matches(&instance)
                && expected.matches_source(native.storage()),
            crate::session::LifecycleReservationError::Superseded
        );
        let _ownership = instance.acquire_auxiliary_locks_in(&native)?;
        anyhow::ensure!(
            expected.matches(&instance),
            crate::session::LifecycleReservationError::Superseded
        );
        instance.ensure_startable()?;
        anyhow::ensure!(
            !instance.is_structured(),
            "Session no longer has a terminal agent"
        );
        let name = target.tmux_session;
        let physical = target
            .physical
            .ok_or(crate::session::NativeInputError::Ownership)?;
        drop(_ownership);
        let fence = crate::session::NativeInputFence::capture(
            &expected.profile,
            &instance.id,
            instance.created_at,
            instance.lifecycle_generation,
            true,
            expected.profile_identity,
        )?;
        let session = instance.agent_input_session(&name, physical, true, Some(fence))?;
        Ok((native, instance, session))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    // Captured native/physical scope now owns admission. User after-send hooks
    // must be able to re-enter lifecycle operations through this daemon too.
    drop(guard);
    drop(_submission);
    drop(namespace);
    let result = match prepared {
        Ok((native, instance, session)) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
                use crate::session::SessionStore;
                session.send_keys_with_delay(
                    &message,
                    crate::agents::send_keys_enter_delay(&instance.tool),
                )?;
                let _lifecycle = native
                    .storage()
                    .acquire_instance_lifecycle_lock(&instance.id)?;
                let recency = (&native as &dyn SessionStore).update(|rows, _| {
                    let row = rows
                        .iter_mut()
                        .find(|row| row.id == instance.id)
                        .ok_or(crate::session::SessionGone)?;
                    anyhow::ensure!(
                        row.created_at == instance.created_at
                            && row.lifecycle_generation == instance.lifecycle_generation,
                        crate::session::LifecycleReservationError::Superseded
                    );
                    row.last_accessed_at = Some(chrono::Utc::now());
                    Ok(())
                });
                if let Err(error) = recency {
                    if error.is::<crate::session::SessionCommitApplied>()
                        || !(error.is::<crate::session::SessionGone>()
                            || error.downcast_ref::<crate::session::LifecycleReservationError>()
                                == Some(&crate::session::LifecycleReservationError::Superseded))
                    {
                        return Err(error);
                    }
                }
                Ok(revived)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        }
        Err(error) => Err(error),
    };
    result.map_err(|error| terminal_send_failure(&state, error))
}

fn terminal_send_failure(state: &crate::server::AppState, error: anyhow::Error) -> SendKeysError {
    if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
        SendKeysError::Blocked(*blocked)
    } else if error.is::<crate::session::SessionGone>() {
        SendKeysError::Gone
    } else if error.is::<TerminalNotRunning>() {
        SendKeysError::NotRunning
    } else if error.is::<crate::session::NativeInputError>() {
        let detail = serde_json::json!({
            "error": "terminal_input_unavailable", "message": error.to_string(),
        });
        SendKeysError::Preparation {
            response: Box::new((StatusCode::CONFLICT, Json(&detail)).into_response()),
            detail,
        }
    } else if let Some(response) = super::lifecycle::lifecycle_rejection(state, &error) {
        SendKeysError::Preparation {
            response: Box::new(response),
            detail: serde_json::json!({"error": "lifecycle_busy"}),
        }
    } else {
        SendKeysError::Tmux(error)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Terminal session is not running")]
struct TerminalNotRunning;

impl IntoResponse for TerminalSendError {
    fn into_response(self) -> axum::response::Response {
        let mut response = match self.error {
            SendKeysError::NotFound | SendKeysError::Gone => bare_not_found(),
            SendKeysError::NotRunning => crate::server::api::api_error_with_code_header(
                crate::daemon::ApiErrorCode::SessionNotRunning,
                "Terminal session is not running",
            ),
            SendKeysError::Preparation { response, .. } => *response,
            SendKeysError::Transient(status) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session_transient", "status": format!("{status:?}"),
                })),
            )
                .into_response(),
            SendKeysError::StructuredView => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "acp_mode_unsupported"})),
            )
                .into_response(),
            SendKeysError::Blocked(blocked) => crate::server::api::start_blocked_response(blocked),
            SendKeysError::Tmux(error) => {
                tracing::warn!(target: "http.api.sessions", %error, "terminal send failed");
                if error.is::<crate::session::NativeStoreUnavailable>() {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "tmux_error",
                        "Terminal send failed",
                    )
                }
            }
            SendKeysError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response(),
        };
        if let Some(cursor) = self.committed_cursor {
            let (epoch, revision) = *cursor;
            response
                .headers_mut()
                .insert(crate::daemon::RUNTIME_EPOCH_HEADER, epoch);
            response
                .headers_mut()
                .insert(crate::daemon::RUNTIME_REVISION_HEADER, revision);
        }
        response
    }
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Terminal keystroke injection: CityHall sessions are structured-view only,
    // so close this explicitly rather than leaning on the downstream error.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let Json(mut req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    crate::server::live_ws::strip_paste_escapes(&mut req.message);
    let len = req.message.trim_end_matches(['\r', '\n']).len();
    req.message.truncate(len);
    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message_empty"})),
        )
            .into_response();
    }

    let revive = req.revive;
    match send_terminal_message(&state.session_service, &id, req.message, revive).await {
        Ok(_) => {
            tracing::info!(target: "session.lifecycle", session = %id, len, revive, "terminal message sent");
            match state.runtime.publish(&state).await {
                Ok(snapshot) => crate::server::runtime::mutation_response(
                    &snapshot.value.cursor,
                    (StatusCode::OK, Json(serde_json::json!({"sent": true}))),
                ),
                Err(error) => {
                    tracing::debug!(target: "server.runtime", %error, "runtime publish failed");
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }
        Err(error) => error.into_response(),
    }
}

/// Max decoded size of a pasted image (5 MiB). Claude Code caps image
/// attachments around this size; the route body limit in `build_router`
/// leaves headroom for base64's ~33% overhead plus JSON framing.
const MAX_PASTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Directory, relative to the session worktree, holding images pasted into
/// the live terminal. It lives inside the worktree so a Docker-sandboxed
/// pane, which mounts the worktree but cannot see the host temp dir, can
/// still read the file. A self-ignoring `.gitignore` keeps the blobs out of
/// git. See #2678.
const PASTE_IMAGE_DIR: &str = ".aoe-pasted-images";

#[derive(Deserialize)]
pub struct PasteImageRequest {
    /// Client-declared MIME. Advisory only: the extension and the
    /// accept/reject decision come from magic-byte sniffing, never this field.
    #[serde(default)]
    pub mime_type: String,
    /// Standard-base64 image bytes.
    pub data: String,
}

fn paste_image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Write the decoded blob into the worktree's paste-image dir and return the
/// host path plus the generated file name. Sync (filesystem I/O); call from a
/// blocking pool.
fn write_paste_image(
    project_path: &str,
    bytes: &[u8],
    ext: &str,
) -> std::io::Result<(std::path::PathBuf, String)> {
    let dir = std::path::Path::new(project_path).join(PASTE_IMAGE_DIR);
    std::fs::create_dir_all(&dir)?;
    // A `.gitignore` of `*` also ignores itself, so the directory stays
    // invisible to `git add` with no git subprocess.
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    let file_name = format!("aoe-paste-{}.{}", uuid::Uuid::new_v4(), ext);
    let path = dir.join(&file_name);
    // create_new: uuid names never collide; fail loud if the impossible happens.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    Ok((path, file_name))
}

/// Map the host paste-image file to the path the tmux pane reads. Non-sandboxed
/// panes share the host filesystem, so the absolute host path is correct. A
/// sandboxed pane mounts the worktree under a container path (`/workspace/...`);
/// reuse `compute_volume_paths` so the pasted path matches that mount.
fn pane_visible_paste_path(project_path: &str, is_sandboxed: bool, file_name: &str) -> String {
    if is_sandboxed {
        if let Ok((_, working_dir)) = crate::session::config::container_config::compute_volume_paths(
            std::path::Path::new(project_path),
            project_path,
        ) {
            return format!("{working_dir}/{PASTE_IMAGE_DIR}/{file_name}");
        }
    }
    std::path::Path::new(project_path)
        .join(PASTE_IMAGE_DIR)
        .join(file_name)
        .to_string_lossy()
        .to_string()
}

/// Save a clipboard image pasted into the live terminal and return the path
/// the tmux pane can read, so the CLI agent (e.g. Claude Code) attaches it.
/// See #2678.
pub async fn paste_image(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PasteImageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    use base64::Engine as _;

    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Allowed for the CityHall composer, but only against a structured session:
    // a terminal target would let a locked-down client write into another
    // session's worktree.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    let Some(instance) = crate::server::api::find_instance(&state, &id).await else {
        return bare_not_found();
    };

    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data.as_bytes()) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_base64"})),
            )
                .into_response();
        }
    };
    if bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "empty"})),
        )
            .into_response();
    }
    if bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "too_large"})),
        )
            .into_response();
    }
    let Some(mime) = crate::server::api::acp::sniff_image_mime(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "not_an_image"})),
        )
            .into_response();
    };
    let ext = paste_image_extension(mime);

    let project_path = instance.project_path.clone();
    let is_sandboxed = instance.is_sandboxed();
    let write_project = project_path.clone();
    let (host_path, file_name) =
        match tokio::task::spawn_blocking(move || write_paste_image(&write_project, &bytes, ext))
            .await
        {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: write failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
        };

    // Cancel only the timer; an in-progress removal must finish.
    let shutdown = state.shutdown.clone();
    state
        .runtime
        .work
        .spawn("server.paste_image_cleanup", async move {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
            }
            let _ = tokio::fs::remove_file(&host_path).await;
        });

    let pane_path = pane_visible_paste_path(&project_path, is_sandboxed, &file_name);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "path": pane_path })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_output_lines")]
    pub lines: u32,
    #[serde(default = "default_output_format")]
    pub format: String,
}

fn default_output_lines() -> u32 {
    200
}

fn default_output_format() -> String {
    "text".to_string()
}

enum CaptureError {
    NotRunning,
    Tmux(anyhow::Error),
}

pub async fn read_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<OutputQuery>,
) -> impl IntoResponse {
    // Raw terminal pane content: CityHall hides the terminal UI and WS relay,
    // so this read must be closed too.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let lines = (q.lines as usize).clamp(1, 2000);
    let want_ansi = match q.format.as_str() {
        "ansi" => true,
        "text" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "format_invalid",
                    "allowed": ["text", "ansi"]
                })),
            )
                .into_response();
        }
    };

    let Some(instance) = crate::server::api::find_instance(&state, &id).await else {
        return bare_not_found();
    };

    let capture_result = tokio::task::spawn_blocking(move || -> Result<String, CaptureError> {
        let tmux_session = instance.tmux_session().map_err(CaptureError::Tmux)?;
        if !tmux_session.exists() {
            return Err(CaptureError::NotRunning);
        }
        let raw = tmux_session
            .capture_pane(lines)
            .map_err(CaptureError::Tmux)?;
        if want_ansi {
            Ok(raw)
        } else {
            Ok(crate::tmux::utils::strip_ansi(&raw))
        }
    })
    .await;

    match capture_result {
        Ok(Ok(content)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id,
                "lines": lines,
                "format": q.format,
                "content": content,
            })),
        )
            .into_response(),
        Ok(Err(CaptureError::NotRunning)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session_not_running"})),
        )
            .into_response(),
        Ok(Err(CaptureError::Tmux(e))) => {
            tracing::error!(target: "http.api.sessions", "read_output: tmux error for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "tmux_error"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "read_output: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod send_output_tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn send_success_invalidates_a_reload_snapshot_from_before_the_mutation() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut live = Instance::new("send-race", "/tmp/send-race");
        live.source_profile = "send-race".into();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "send-race",
            vec![live.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![live.clone()]);
        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)
                .unwrap()
                .metadata;
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let mut stale = live.clone();
        stale.title = "stale disk row".into();
        stale.last_accessed_at = None;

        let worker_state = state.clone();
        let id = live.id.clone();
        tokio::task::spawn_blocking(move || {
            use crate::session::SessionStore;
            let native = crate::server::session_store::NativeSessionStore::open(
                worker_state,
                "send-race",
                None,
            )
            .unwrap();
            let store: &dyn SessionStore = &native;
            store
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .touch_after_input();
                    Ok(())
                })
                .unwrap();
        })
        .await
        .unwrap();
        assert_eq!(
            state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst),
            read_epoch + 1
        );

        let metadata = state.canonical_metadata.read().await.clone();
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            (vec![stale]).into(),
            Vec::new(),
            crate::server::state::StatusSource::DiskOnly,
            read_epoch,
            Ok(metadata),
            Default::default(),
        )
        .await;

        let current = state.instances.read().await;
        assert_eq!(current[0].title, live.title);
        assert!(current[0].last_accessed_at.is_some());
    }

    #[tokio::test]
    async fn post_preparation_send_failure_preserves_receipt_and_http_error() {
        let response = TerminalSendError {
            error: SendKeysError::NotRunning,
            committed_cursor: Some(Box::new((
                "committed-epoch".parse().unwrap(),
                "19".parse().unwrap(),
            ))),
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.headers()[crate::daemon::ERROR_CODE_HEADER],
            "session_not_running"
        );
        assert_eq!(
            response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
            "committed-epoch"
        );
        assert_eq!(
            response.headers()[crate::daemon::RUNTIME_REVISION_HEADER],
            "19"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["error"],
            "session_not_running"
        );

        let response = TerminalSendError {
            error: SendKeysError::Preparation {
                response: Box::new((
                    StatusCode::CONFLICT,
                    crate::daemon::ApiErrorCode::ResumeFailed.header(),
                    Json(serde_json::json!({"error": "resume_failed", "resume_session_id": "sid"})),
                )
                    .into_response()),
                detail: serde_json::json!({"error": "resume_failed"}),
            },
            committed_cursor: None,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.headers()[crate::daemon::ERROR_CODE_HEADER],
            "resume_failed"
        );
    }
}

#[cfg(test)]
mod paste_image_tests {
    use super::*;
    use tempfile::tempdir;

    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[test]
    fn write_paste_image_lands_in_worktree_and_ignores_itself() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let (path, name) = write_paste_image(&project, PNG_1PX, "png").unwrap();

        assert!(path.exists(), "image file must be written");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            PNG_1PX,
            "bytes must round-trip"
        );
        assert!(name.starts_with("aoe-paste-") && name.ends_with(".png"));
        let gitignore = dir.path().join(PASTE_IMAGE_DIR).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(gitignore).unwrap(),
            "*\n",
            "dir must self-ignore so pasted blobs never reach git"
        );
    }

    #[test]
    fn pane_path_is_host_path_or_container_mount() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let dir_name = dir.path().file_name().unwrap().to_string_lossy();

        assert_eq!(
            pane_visible_paste_path(&project, false, "aoe-paste-x.png"),
            dir.path()
                .join(PASTE_IMAGE_DIR)
                .join("aoe-paste-x.png")
                .to_string_lossy()
        );
        // A non-git worktree mounts under /workspace/<dir-name>; the pasted
        // path must be the container-visible path, not the host path.
        assert_eq!(
            pane_visible_paste_path(&project, true, "aoe-paste-x.png"),
            format!("/workspace/{dir_name}/{PASTE_IMAGE_DIR}/aoe-paste-x.png")
        );
    }
}

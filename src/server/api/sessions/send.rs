//! Send-message, paste-image, and read-output endpoints.
//!
//! Send plus read-output are the minimum primitive an external orchestrator
//! needs to drive an aoe session as a controlled subagent, without a keyboard
//! or websocket attach.

use super::*;

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    /// Auto-revive a dead/stopped session before sending. Defaults to `true`;
    /// `false` is fail-loud, matching the `--no-revive` CLI flag.
    #[serde(default = "default_revive")]
    pub revive: bool,
}

fn default_revive() -> bool {
    true
}

enum SendKeysError {
    NotRunning,
    ResumeFailed(String),
    Transient(Status),
    StructuredView,
    Tmux(anyhow::Error),
}

type SendKeysResult =
    Result<(EnsureReadyOutcome, Instance), Box<(Instance, EnsureReadyOutcome, SendKeysError)>>;

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
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message_empty"})),
        )
            .into_response();
    }

    // Serialize concurrent sends (and other tmux mutations) for this id, or two
    // racing POSTs interleave their bytes inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let Some(instance) = find_instance(&state, &id).await else {
        return bare_not_found();
    };

    let sync_base = instance.clone();
    let tool = instance.tool.clone();
    let message = req.message;
    let revive = req.revive;
    let send_result = tokio::task::spawn_blocking(move || -> SendKeysResult {
        // Revive the pane before sending, unless the caller opted out: a send
        // to a dead pane silently writes keystrokes to a corpse.
        //
        // The Err arm surfaces both `inst_owned` and the `EnsureReadyOutcome`,
        // so the caller can sync post-resume-path mutations back to live state
        // whichever failure path fires, and can tell a cascade-fired outcome
        // from the no-op `AlreadyAlive` one. Without that split, the
        // `revive=false + NotRunning` path would clobber live `last_error`.
        let mut inst_owned = instance;
        let outcome = if revive {
            match inst_owned.ensure_pane_ready() {
                Ok(o) => o,
                Err(e) => {
                    let mapped = match e {
                        EnsureReadyError::Transient(s) => SendKeysError::Transient(s),
                        EnsureReadyError::StructuredView => SendKeysError::StructuredView,
                        EnsureReadyError::Tmux(e) => SendKeysError::Tmux(e),
                    };
                    // Tagged AlreadyAlive because ensure_pane_ready did not
                    // mutate user-visible state here, keeping the outer
                    // `did_work` flag false. `EnsureReadyError::Tmux` covers
                    // both an unmutated pre-cascade failure and a committed
                    // post-resume-path one, so its outer arm syncs
                    // unconditionally; the others bail before any mutation.
                    return Err(Box::new((
                        inst_owned,
                        EnsureReadyOutcome::AlreadyAlive,
                        mapped,
                    )));
                }
            }
        } else {
            EnsureReadyOutcome::AlreadyAlive
        };
        if let EnsureReadyOutcome::ResumeFailed { sid } = &outcome {
            return Err(Box::new((
                inst_owned,
                outcome.clone(),
                SendKeysError::ResumeFailed(sid.clone()),
            )));
        }
        let tmux_session = match inst_owned.tmux_session() {
            Ok(s) => s,
            Err(e) => return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e)))),
        };
        if !tmux_session.exists() {
            return Err(Box::new((inst_owned, outcome, SendKeysError::NotRunning)));
        }
        let delay = crate::agents::send_keys_enter_delay(&tool);
        if let Err(e) = tmux_session.send_keys_with_delay(&message, delay) {
            return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e))));
        }
        Ok((outcome, inst_owned))
    })
    .await;

    match send_result {
        Ok(Ok((outcome, started))) => {
            let body = serde_json::json!({"sent": true});
            // ensure_pane_ready mutated `started` on the clone, so sync it back
            // or a rapid follow-up generates a fresh `agent_session_id` and
            // orphans the prior Claude conversation. See `apply_post_restart_sync`.
            // Also stamps last_accessed_at for the activity column.
            let mut instances = state.instances.write().await;
            let profile = if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                if !matches!(outcome, EnsureReadyOutcome::AlreadyAlive) {
                    apply_post_restart_sync(i, &sync_base, &started);
                }
                i.touch_last_accessed();
                i.source_profile.clone()
            } else {
                // Deleted between the send and the stamp; nothing to persist.
                return (StatusCode::OK, Json(body)).into_response();
            };
            drop(instances);
            let id_for_save = id.clone();
            let sync_base_for_save = sync_base.clone();
            let started_for_save = started.clone();
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile, state.file_watch.clone()) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id_for_save) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(
                                    disk_inst,
                                    &sync_base_for_save,
                                    &started_for_save,
                                );
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // Anything other than AlreadyAlive means ensure_pane_ready touched
            // fields the live entry needs (fresh sid, last_start_time), so sync
            // only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match send_err {
                SendKeysError::NotRunning => {
                    // An external kill or a remain-on-exit-off crash can race
                    // ensure_pane_ready's Alive decision. Use the narrow sync
                    // helper so status and last_error stay untouched: NotRunning
                    // is recoverable, and `Starting` from finalize_launch would
                    // briefly mis-paint a broken pane.
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": "session_not_running"})),
                    )
                        .into_response()
                }
                SendKeysError::ResumeFailed(sid) => {
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &sync_base, &started);
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "resume_failed",
                            "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                            "resume_session_id": sid,
                        })),
                    )
                        .into_response()
                }
                SendKeysError::Transient(status) => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "session_transient",
                        "status": format!("{status:?}"),
                    })),
                )
                    .into_response(),
                SendKeysError::StructuredView => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "acp_mode_unsupported"})),
                )
                    .into_response(),
                SendKeysError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", "send_message: tmux error for {id}: {e}");
                    let msg = e.to_string();
                    // Mirror `ensure_session`'s Err arm: full sync, then
                    // override `status` and `last_error` so observers do not see
                    // `Status::Starting` on a broken session. Tmux Err is the
                    // catch-all for both an unmutated pre-cascade failure and a
                    // post-resume-path one whose durable state must be copied
                    // back from the clone.
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            i.status = crate::session::Status::Error;
                            i.last_error = Some(msg);
                        }
                    }
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "tmux_error"})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "send_message: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

/// Max decoded size of a pasted image (5 MiB), matching Claude Code's
/// attachment cap. The route body limit leaves headroom for base64 overhead.
const MAX_PASTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Directory, relative to the session worktree, holding images pasted into the
/// live terminal. Inside the worktree so a Docker-sandboxed pane, which cannot
/// see the host temp dir, can still read the file. A self-ignoring `.gitignore`
/// keeps the blobs out of git (#2678).
const PASTE_IMAGE_DIR: &str = ".aoe-pasted-images";

#[derive(Deserialize)]
pub struct PasteImageRequest {
    /// Client-declared MIME. Advisory only: the extension and the
    /// accept/reject decision come from magic-byte sniffing.
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
/// host path plus the generated file name. Sync I/O; call from a blocking pool.
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

/// Map the host paste-image file to the path the tmux pane reads. A sandboxed
/// pane mounts the worktree under a container path, so `compute_volume_paths`
/// is reused to match that mount.
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

/// Save a clipboard image pasted into the live terminal and return the path the
/// tmux pane can read, so the CLI agent attaches it (#2678).
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

    let Some(instance) = find_instance(&state, &id).await else {
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

    // Best-effort TTL cleanup: the file only needs to outlive the agent reading
    // it, and a detached task avoids teardown bookkeeping.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
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

    let Some(instance) = find_instance(&state, &id).await else {
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

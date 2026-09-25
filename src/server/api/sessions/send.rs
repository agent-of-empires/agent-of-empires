//! Send-message, paste-image, and read-output endpoints.

use super::*;

// ============================================================================
// Send + read-output endpoints
//
// Together these are the minimum primitive an external orchestrator needs to
// run an aoe session as a controlled subagent: push a prompt in, read the
// pane back. Mirrors what the TUI's send-message dialog and pane preview do,
// without requiring keyboard or websocket attach.
// ============================================================================

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

enum SendKeysError {
    NotRunning,
    ResumeFailed(String),
    Transient(Status),
    StructuredView,
    Tmux(anyhow::Error),
}

type SendKeysResult =
    Result<(EnsureReadyOutcome, Instance), Box<(Instance, EnsureReadyOutcome, SendKeysError)>>;

async fn commit_send_success_to_live(
    state: &Arc<AppState>,
    id: &str,
    sync_base: &Instance,
    started: &Instance,
    restarted: bool,
) -> Option<String> {
    let mut instances = state.instances.write().await;
    let instance = instances.iter_mut().find(|instance| instance.id == id)?;
    if restarted {
        apply_post_restart_sync(instance, sync_base, started);
    }
    instance.touch_last_accessed();
    // Keep the epoch transition inside the same critical section as the row
    // mutation. A disk reload that captured the prior epoch either finishes
    // before this lock or observes the bump while holding it and rejects its
    // stale snapshot.
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Some(instance.source_profile.clone())
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Terminal keystroke injection: CityHall sessions are structured-view only
    // (the composer drives the agent via the ACP prompt route), so close this
    // explicitly rather than leaning on the downstream StructuredView error.
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

    // Serialize concurrent sends (and other tmux mutations) for this id.
    // Without this, two POSTs racing against the same session would issue
    // overlapping `tmux send-keys -l` invocations and the bytes can interleave
    // inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let sync_base = instance.clone();
    let tool = instance.tool.clone();
    let message = req.message;
    let revive = req.revive;
    let send_result = tokio::task::spawn_blocking(move || -> SendKeysResult {
        // Revive the pane before sending. Without this, a send to a dead
        // pane silently writes keystrokes to a corpse with no agent.
        // Skipped when the caller opts out via `revive: false`.
        //
        // The closure surfaces both `inst_owned` AND the
        // `EnsureReadyOutcome` on the Err arm so the caller can sync
        // post-resume-path mutations (`agent_session_id`, failure marker,
        // and `retroactive_capture_excludes`) back to live state regardless
        // of which failure path fires. The
        // outcome lets the caller distinguish cascade-fired
        // (`Respawned`/`Started`) from the no-op `AlreadyAlive` path
        // so a sync only happens when there's actual cascade state to
        // propagate; this avoids clobbering live `last_error` on the
        // `revive=false + NotRunning` path where `started` is
        // unmutated.
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
                    // ensure_pane_ready did not mutate user-visible
                    // state via the outcome path. Tag as AlreadyAlive
                    // so the outer match's `did_work` flag stays
                    // false. `EnsureReadyError::Tmux` may be either
                    // pre-cascade (tmux_session() / start_with_size
                    // subprocess failure: `inst_owned` unmutated) or
                    // post-resume-path (mutations committed).
                    // The Tmux outer arm syncs unconditionally and
                    // covers both shapes; the others (Transient /
                    // StructuredView) bail before any mutation.
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
            // ensure_pane_ready mutated `started` (status, agent_session_id,
            // last_start_time, last_error) on the clone. Sync those back to
            // the live entry so the next request sees a coherent view;
            // without this, a rapid follow-up could generate a fresh
            // `agent_session_id` and orphan the prior Claude conversation.
            // See `apply_post_restart_sync`. Also stamp last_accessed_at so
            // the activity column reflects API-driven interaction.
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            let Some(profile) = commit_send_success_to_live(
                &state,
                &id,
                &sync_base,
                &started,
                !outcome_already_alive,
            )
            .await
            else {
                // Session was deleted between the send and the stamp; nothing
                // left to persist.
                return (StatusCode::OK, Json(body)).into_response();
            };
            let state_for_persist = Arc::clone(&state);

            let persisted = tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile, state_for_persist.file_watch.clone()) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(
                                    disk_inst,
                                    &sync_base,
                                    &started,
                                );
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            }).await;
            if let Err(error) = persisted {
                tracing::warn!(target: "http.api.sessions", %error, "send_message persistence task failed");
            }
            state.runtime.request_publish();
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // ensure_pane_ready did mutate state when the outcome is
            // anything other than AlreadyAlive. `Started` and `Respawned`
            // touch fields the live entry needs to reflect (fresh sid from
            // acquire, last_start_time, etc.). Sync only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match send_err {
                SendKeysError::NotRunning => {
                    // External kill or remain-on-exit-off crash can race
                    // ensure_pane_ready's Alive decision against the
                    // tmux_session.exists() check. Propagate resume-path
                    // state when applicable; use the narrow sync helper to
                    // leave status and last_error untouched (NotRunning is
                    // recoverable; `started.status = Starting` from
                    // finalize_launch would briefly mis-paint a broken pane).
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                            state
                                .mutation_epoch
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            state.runtime.request_publish();
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
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            state
                                .mutation_epoch
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            state.runtime.request_publish();
                        }
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
                    // Sync cascade-mutated fields back to live state. Mirror
                    // `ensure_session`'s Err arm: full sync, then override
                    // `status` and `last_error` so observers don't see
                    // `Status::Starting` (set by `finalize_launch`) on a
                    // broken session. Tmux Err is the
                    // catch-all for both pre-cascade tmux failures (where
                    // `started` is unmutated and the sync is a no-op) and
                    // post-resume-path failures (where durable resume state
                    // must be copied back from the clone).
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            i.status = crate::session::Status::Error;
                            i.last_error = Some(msg);
                            state
                                .mutation_epoch
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            state.runtime.request_publish();
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
    // A `.gitignore` of `*` also ignores itself, so the whole directory stays
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
    // Allowed for the CityHall composer, but only against a structured
    // session: a plain/terminal target would let a locked-down client write
    // into another session's worktree.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

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
    // Raw terminal pane content: CityHall hides the terminal UI + WS relay, so
    // this read must be closed too or the pane is reachable by session id.
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

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

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

    #[test]
    fn output_query_default_constants() {
        assert_eq!(default_output_lines(), 200);
        assert_eq!(default_output_format(), "text");
    }

    #[test]
    fn send_message_request_requires_message_field() {
        let r: Result<SendMessageRequest, _> = serde_json::from_str("{}");
        assert!(r.is_err(), "missing message must reject");
    }

    #[test]
    fn send_message_request_accepts_message() {
        let r: SendMessageRequest = serde_json::from_str("{\"message\":\"hello\"}").unwrap();
        assert_eq!(r.message, "hello");
    }

    #[tokio::test]
    async fn send_success_invalidates_a_reload_snapshot_from_before_the_mutation() {
        let live = Instance::new("send-race", "/tmp/send-race");
        let state = crate::server::test_support::build_test_app_state(vec![live.clone()]);
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let mut stale = live.clone();
        stale.title = "stale disk row".into();
        stale.last_accessed_at = None;

        assert_eq!(
            commit_send_success_to_live(&state, &live.id, &live, &live, false)
                .await
                .as_deref(),
            Some(live.source_profile.as_str())
        );
        assert_eq!(
            state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst),
            read_epoch + 1
        );

        let metadata = state.canonical_metadata.read().await.clone();
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            vec![stale],
            Vec::new(),
            crate::server::state::StatusSource::DiskOnly,
            read_epoch,
            metadata,
            Default::default(),
        )
        .await;

        let current = state.instances.read().await;
        assert_eq!(current[0].title, live.title);
        assert!(current[0].last_accessed_at.is_some());
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
    fn extension_from_sniffed_mime() {
        assert_eq!(paste_image_extension("image/png"), "png");
        assert_eq!(paste_image_extension("image/jpeg"), "jpg");
        assert_eq!(paste_image_extension("image/gif"), "gif");
        assert_eq!(paste_image_extension("image/webp"), "webp");
    }

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
    fn non_sandboxed_pane_path_is_absolute_host_path() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let pane = pane_visible_paste_path(&project, false, "aoe-paste-x.png");

        let expected = dir
            .path()
            .join(PASTE_IMAGE_DIR)
            .join("aoe-paste-x.png")
            .to_string_lossy()
            .to_string();
        assert_eq!(pane, expected);
    }

    #[test]
    fn sandboxed_pane_path_uses_container_mount() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let dir_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let pane = pane_visible_paste_path(&project, true, "aoe-paste-x.png");

        // A non-git worktree mounts under /workspace/<dir-name>; the pasted
        // path must be the container-visible path, not the host path.
        assert_eq!(
            pane,
            format!("/workspace/{dir_name}/{PASTE_IMAGE_DIR}/aoe-paste-x.png")
        );
    }
}

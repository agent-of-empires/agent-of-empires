//! Rich diff, file-contents, and volume-ignores preview endpoints.

use super::*;

// --- Rich Diff (per-file, merge-base aware) ---

#[derive(Serialize)]
pub struct RichDiffFileInfo {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    /// Name of the workspace repo this file belongs to. None for
    /// single-repo (non-workspace) sessions. The frontend uses this to
    /// group entries in the sidebar diff list and to disambiguate
    /// path collisions across repos. See #1047.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
}

#[derive(Serialize)]
pub struct RepoBase {
    /// None for single-repo sessions; Some for each workspace member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub base_branch: String,
    /// Worktree path this entry's diff was computed in. The web base
    /// picker queries it for that repo's branch list, so a workspace
    /// member's typeahead lists its own branches rather than the launch
    /// repo's. See #3329.
    pub repo_path: String,
    /// This entry's explicit override, when one is set. Absent means
    /// `base_branch` came from the recorded creation base, the profile
    /// default, or auto-detection, so the client hides its reset
    /// affordance. See #3329.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_override: Option<String>,
}

#[derive(Serialize)]
pub struct RichDiffFilesResponse {
    pub files: Vec<RichDiffFileInfo>,
    /// One entry per repo whose diff was computed. Single-repo
    /// sessions get a one-element array with `repo_name: None`;
    /// workspace sessions get one entry per workspace member. Replaces
    /// the previous single-string `base_branch` since each member can
    /// have a different default. See #1047.
    pub per_repo_bases: Vec<RepoBase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Contents-based diff response: raw old/new text that the web client parses
/// and renders itself via `@pierre/diffs`. See [`MAX_CONTENTS_BYTES`].
#[derive(Serialize)]
pub struct RichFileContentsResponse {
    pub file: RichDiffFileInfo,
    pub old_content: String,
    pub new_content: String,
    /// Server-computed unified diff of old → new. The client parses this as
    /// text (`parsePatchFiles`) instead of re-diffing the contents, which
    /// would block the main thread on large files. Empty for binary files.
    pub patch: String,
    pub is_binary: bool,
    /// True if the file was too large to send inline; contents are omitted.
    pub truncated: bool,
}

/// Caps for the contents-based diff endpoint. The client renders with a
/// virtualized, off-main-thread highlighter (`@pierre/diffs`), so the DOM and
/// main thread are no longer the bottleneck; the only real cost is JSON
/// payload size and the client-side parse. The byte cap is the real guard
/// against pathological payloads (minified bundles, generated code, data
/// blobs); the line cap is a secondary backstop.
const MAX_CONTENTS_BYTES: usize = 5_000_000;
const MAX_CONTENTS_LINES: usize = 200_000;

/// Validate a user-supplied relative file path against a workdir.
///
/// Returns `(canonical_path, is_changed)` if the requested path is safe to read
/// (no absolute, no `..`, no symlink-escape out of the workdir). `is_changed`
/// is true when the path appears in `changed_files` (diffable); false marks an
/// in-repo file with no diff against the base, served via the full-file
/// fallback (gated further on being a tracked blob; see
/// [`crate::git::diff::compute_unchanged_file_contents`]). See #1810.
///
/// A path that is neither in the changed set nor present on disk yields
/// `NOT_FOUND`. The non-canonical fallback is reserved for the changed-set case
/// (a file deleted in the working tree but still diffable); the unchanged
/// branch requires canonicalization to succeed. Returns `Err(status, message)`
/// otherwise.
pub(super) fn validate_diff_path(
    workdir: &std::path::Path,
    requested: &std::path::Path,
    changed_files: &[crate::git::diff::DiffFile],
) -> Result<(std::path::PathBuf, bool), (StatusCode, &'static str)> {
    use std::path::Component;

    if requested.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty path"));
    }
    if requested.is_absolute() {
        return Err((StatusCode::BAD_REQUEST, "absolute path not allowed"));
    }
    for comp in requested.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((StatusCode::BAD_REQUEST, "path escapes workdir"));
            }
            _ => {}
        }
    }

    let is_changed = changed_files.iter().any(|f| f.path == requested);

    // Canonicalize both sides and verify containment as defense in depth
    // against symlinks that might point outside the workdir.
    let canonical_workdir = workdir.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "workdir canonicalize failed",
        )
    })?;
    let full = canonical_workdir.join(requested);
    match full.canonicalize() {
        Ok(c) => {
            if !c.starts_with(&canonical_workdir) {
                return Err((StatusCode::BAD_REQUEST, "path escapes workdir"));
            }
            Ok((c, is_changed))
        }
        // The file isn't on disk. A changed file may have been deleted in the
        // working tree but is still diffable, so fall back to the non-canonical
        // (component-vetted) path. An unchanged path that isn't on disk has
        // nothing to show.
        Err(_) if is_changed => Ok((full, true)),
        Err(_) => Err((StatusCode::NOT_FOUND, "file not found")),
    }
}

/// One repo's worth of diff context: a name (for workspace members),
/// the filesystem path the diff helper walks, and the two base-branch
/// layers that vary per repo. See #1047, #3329.
#[derive(Clone, Debug)]
pub(super) struct DiffRepo {
    /// Workspace member name, or None for single-repo sessions.
    pub(super) name: Option<String>,
    pub(super) path: String,
    /// Explicit override for this entry's diff base, set via
    /// `PATCH /api/sessions/{id}/diff-base`, the `aoe session set-base`
    /// CLI, or the TUI diff view's `b` keybind. For a workspace member
    /// that is `WorkspaceRepo::base_branch_override`; for a single-repo
    /// session's own checkout it is `Instance::base_branch_override`.
    /// See #970, #3329.
    pub(super) base_override: Option<String>,
    /// The branch this entry's worktree was created from, recorded at
    /// creation. `WorkspaceRepo::base_branch` for a workspace member,
    /// `WorktreeInfo::base_branch` for a single-repo session. Slots
    /// below the explicit override but above the profile default and
    /// auto-detection. See #1951, #3329.
    pub(super) recorded_base: Option<String>,
}

struct DiffContext {
    repos: Vec<DiffRepo>,
}

/// Expand a session into the list of repos whose diffs the sidebar
/// cares about. Workspace sessions iterate `workspace_info.repos`
/// (each `worktree_path` becomes one entry); single-repo sessions
/// fall back to a one-element list of `[project_path]` so the
/// existing flow is unchanged. See #1047.
async fn resolve_diff_repos(
    state: &AppState,
    id: &str,
) -> Result<DiffContext, axum::response::Response> {
    let instances = state.instances.read().await;
    let inst = instances
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(crate::server::api::session_not_found)?;
    Ok(DiffContext {
        repos: diff_repos_of(inst),
    })
}

/// The repo entries for one session, split out of [`resolve_diff_repos`] so the
/// per-repo base plumbing is testable without an `AppState`.
pub(super) fn diff_repos_of(inst: &crate::session::Instance) -> Vec<DiffRepo> {
    // A session with any repo record (a creation-time workspace, repos attached
    // later, or both) lists one entry per repo. A session with none falls back
    // to its project_path, which is the single-repo flow unchanged.
    let mut repos: Vec<DiffRepo> = inst
        .all_repos()
        .iter()
        .map(|r| DiffRepo {
            name: Some(r.name.clone()),
            path: r.worktree_path.clone(),
            base_override: r.base_branch_override.clone(),
            recorded_base: r.base_branch.clone(),
        })
        .collect();
    if inst.workspace_info.is_none() {
        // A session with no repo records is single-repo: its own checkout is
        // the only entry, and the session-level override is that entry's
        // override. `attach_project` converts a session into a workspace, so
        // a named entry and this unnamed one never coexist. See #3329.
        repos.insert(
            0,
            DiffRepo {
                name: None,
                path: inst.project_path.clone(),
                base_override: inst.base_branch_override.clone(),
                recorded_base: inst
                    .worktree_info
                    .as_ref()
                    .and_then(|w| w.base_branch.clone()),
            },
        );
    }
    repos
}

/// Resolve the diff base for one repo. The repo's own override wins
/// over the base its worktree was recorded as forked from, which wins
/// over the profile's `DiffConfig.default_branch`, which wins over
/// auto-detection (`get_default_base_ref`). Every layer above the
/// config default is per repo, so each workspace member resolves
/// independently. See #970, #1951, #3329.
pub(super) fn resolve_diff_base(
    override_value: Option<&str>,
    recorded_base: Option<&str>,
    config_default: Option<&str>,
    repo_path: &std::path::Path,
) -> String {
    if let Some(v) = override_value.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = recorded_base.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = config_default.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    crate::git::diff::get_default_base_ref(repo_path).unwrap_or_else(|_| "main".to_string())
}

pub async fn session_diff_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let scan_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        use crate::git::diff;

        let config_default = crate::session::Config::load_or_warn()
            .diff
            .default_branch
            .clone();
        let mut all_files: Vec<RichDiffFileInfo> = Vec::new();
        let mut per_repo_bases: Vec<RepoBase> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for repo in &ctx.repos {
            let path = std::path::Path::new(&repo.path);
            let base_branch = resolve_diff_base(
                repo.base_override.as_deref(),
                repo.recorded_base.as_deref(),
                config_default.as_deref(),
                path,
            );
            let warning = diff::check_merge_base_status(path, &base_branch);
            let changed = scan_state
                .changed_files_cached(path, &base_branch)
                .unwrap_or_default();

            for f in changed {
                all_files.push(RichDiffFileInfo {
                    path: f.path.to_string_lossy().to_string(),
                    old_path: f.old_path.map(|p| p.to_string_lossy().to_string()),
                    status: f.status.label().to_string(),
                    additions: f.additions,
                    deletions: f.deletions,
                    repo_name: repo.name.clone(),
                });
            }
            per_repo_bases.push(RepoBase {
                repo_name: repo.name.clone(),
                base_branch: base_branch.clone(),
                repo_path: repo.path.clone(),
                base_override: repo.base_override.clone(),
            });
            if let Some(w) = warning {
                match repo.name.as_deref() {
                    Some(n) => warnings.push(format!("{n}: {w}")),
                    None => warnings.push(w),
                }
            }
        }

        RichDiffFilesResponse {
            files: all_files,
            per_repo_bases,
            warning: if warnings.is_empty() {
                None
            } else {
                Some(warnings.join("\n"))
            },
        }
    })
    .await;

    match result {
        Ok(resp) => (
            StatusCode::OK,
            Json(serde_json::to_value(resp).expect("RichDiffFilesResponse is always serializable")),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Diff files panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct FileDiffQuery {
    pub path: String,
    /// Workspace repo name when the session is a multi-repo workspace.
    /// Omitted for single-repo sessions; if a workspace session omits
    /// it, the handler defaults to the first member so the legacy
    /// single-repo URL keeps working for the primary repo. See #1047.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Response for a rejected diff request (bad path, file not changed, etc.).
enum DiffFileError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Internal(anyhow::Error),
}

pub async fn session_diff_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<FileDiffQuery>,
) -> impl IntoResponse {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    // Pick the workspace member named in `?repo=`. When the param is
    // missing we default to the first member, which matches the
    // legacy single-repo URL contract (`?path=...` against the
    // session's primary repo). When the named repo doesn't exist, the
    // request is rejected so a stale link doesn't quietly diff the
    // wrong repo. See #1047.
    let selected_repo = match query.repo.as_deref() {
        Some(name) => match ctx.repos.iter().find(|r| r.name.as_deref() == Some(name)) {
            Some(r) => r.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "bad_request",
                        "message": "unknown workspace repo"
                    })),
                )
                    .into_response();
            }
        },
        // A workspace row can persist with `repos: []`; without this arm
        // the omitted-`repo` default would panic on the empty list.
        None => match ctx.repos.first() {
            Some(r) => r.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "bad_request",
                        "message": "workspace has no repos"
                    })),
                )
                    .into_response();
            }
        },
    };
    let project_path = selected_repo.path;
    let selected_repo_name = selected_repo.name;
    let base_override = selected_repo.base_override;
    let recorded_base = selected_repo.recorded_base;
    let scan_state = state.clone();

    let result =
        tokio::task::spawn_blocking(move || -> Result<serde_json::Value, DiffFileError> {
            use crate::git::diff;

            let repo_path = std::path::Path::new(&project_path);
            let file_path = std::path::Path::new(&query.path);

            let config_default = crate::session::Config::load_or_warn()
                .diff
                .default_branch
                .clone();
            let base_branch = resolve_diff_base(
                base_override.as_deref(),
                recorded_base.as_deref(),
                config_default.as_deref(),
                repo_path,
            );

            // Validate the requested path. Files in the changed set are diffed;
            // an in-repo file with no diff against the base is served through
            // the full-file fallback below. The path-traversal and containment
            // checks are the security boundary preventing arbitrary reads.
            let changed_files = scan_state
                .changed_files_cached(repo_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            let (canonical_path, is_changed) =
                match validate_diff_path(repo_path, file_path, &changed_files) {
                    Ok(v) => v,
                    Err((status, msg)) => {
                        return Err(if status == StatusCode::NOT_FOUND {
                            DiffFileError::NotFound(msg)
                        } else {
                            DiffFileError::BadRequest(msg)
                        });
                    }
                };

            // Full-file fallback: an agent-cited file with no diff against the
            // base. Render its current contents instead of a dead end. See #1810.
            if !is_changed {
                let full =
                    diff::compute_unchanged_file_contents(repo_path, file_path, &canonical_path)
                        .map_err(|e| DiffFileError::Internal(e.into()))?
                        .ok_or(DiffFileError::NotFound("file not found"))?;
                let file = RichDiffFileInfo {
                    path: query.path.clone(),
                    old_path: None,
                    status: "unchanged".to_string(),
                    additions: 0,
                    deletions: 0,
                    repo_name: selected_repo_name.clone(),
                };
                let total_lines = full.content.lines().count();
                let resp = if full.content.len() > MAX_CONTENTS_BYTES
                    || total_lines > MAX_CONTENTS_LINES
                {
                    RichFileContentsResponse {
                        file,
                        old_content: String::new(),
                        new_content: String::new(),
                        patch: String::new(),
                        is_binary: full.is_binary,
                        truncated: true,
                    }
                } else {
                    RichFileContentsResponse {
                        file,
                        old_content: String::new(),
                        new_content: full.content,
                        patch: String::new(),
                        is_binary: full.is_binary,
                        truncated: false,
                    }
                };
                return Ok(serde_json::to_value(resp)
                    .expect("RichFileContentsResponse is always serializable"));
            }

            // Hand the client raw old/new text plus a server-computed unified
            // patch. `@pierre/diffs` parses and renders that patch client-side
            // (virtualized, off-main-thread highlighting) without re-running
            // the diff algorithm in the browser.
            let contents = diff::compute_file_contents(repo_path, file_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            // additions/deletions aren't computed on this path; reuse the counts
            // the changed-files scan already produced for the sidebar.
            let (additions, deletions) = changed_files
                .iter()
                .find(|f| f.path == *file_path)
                .map(|f| (f.additions, f.deletions))
                .unwrap_or((0, 0));
            let file = RichDiffFileInfo {
                path: contents.path.to_string_lossy().to_string(),
                old_path: contents.old_path.map(|p| p.to_string_lossy().to_string()),
                status: contents.status.label().to_string(),
                additions,
                deletions,
                repo_name: selected_repo_name.clone(),
            };
            let total_bytes =
                contents.old_content.len() + contents.new_content.len() + contents.patch.len();
            let total_lines =
                contents.old_content.lines().count() + contents.new_content.lines().count();
            let resp = if total_bytes > MAX_CONTENTS_BYTES || total_lines > MAX_CONTENTS_LINES {
                RichFileContentsResponse {
                    file,
                    old_content: String::new(),
                    new_content: String::new(),
                    patch: String::new(),
                    is_binary: contents.is_binary,
                    truncated: true,
                }
            } else {
                RichFileContentsResponse {
                    file,
                    old_content: contents.old_content,
                    new_content: contents.new_content,
                    patch: contents.patch,
                    is_binary: contents.is_binary,
                    truncated: false,
                }
            };
            Ok(
                serde_json::to_value(resp)
                    .expect("RichFileContentsResponse is always serializable"),
            )
        })
        .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err(DiffFileError::BadRequest(msg))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_request", "message": msg})),
        )
            .into_response(),
        Ok(Err(DiffFileError::NotFound(msg))) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found", "message": msg})),
        )
            .into_response(),
        Ok(Err(DiffFileError::Internal(e))) => {
            tracing::error!(target: "http.api.sessions", "File diff failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "diff_failed", "message": "Failed to compute file diff"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "File diff panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SessionFileQuery {
    pub path: String,
}

/// Response for the session file-read endpoint. Mirrors the typed shape of its
/// sibling [`RichFileContentsResponse`]; `content` is empty for a binary or
/// truncated file (the client renders a notice instead).
#[derive(Serialize)]
pub struct SessionFileResponse {
    pub content: String,
    pub is_binary: bool,
    pub truncated: bool,
}

/// Read a session file for the dashboard file viewer (#3088).
///
/// Git-agnostic (works on non-git scratch sessions). A read is allowed when the
/// canonical target is under a session project root (project_path + worktree
/// paths) or is a path the agent touched this session, recovered from the ACP
/// event log. Confinement and bounded reading live in the private
/// `file_provenance` module.
pub async fn session_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SessionFileQuery>,
) -> impl IntoResponse {
    // Reads workspace file contents: the same code-inspection surface as the
    // diff reads, and the Files pane is hidden in CityHall, so close it too.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let project_paths: Vec<std::path::PathBuf> = ctx
        .repos
        .iter()
        .map(|r| std::path::PathBuf::from(&r.path))
        .collect();
    let store = state.acp_event_store.clone();
    let session_id = id.clone();
    let requested = query.path.clone();

    let result = tokio::task::spawn_blocking(move || {
        // Canonicalize project roots up front; a root that no longer resolves
        // is dropped so a stale worktree can't break or widen confinement.
        let roots: Vec<std::path::PathBuf> = project_paths
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();

        // Provenance fallback: page the whole session log and collect the paths
        // the agent touched. Deferred behind a closure so it runs only when the
        // target is outside every project root; a workspace file (the common
        // case) never pays for the replay.
        // ponytail: per-request scan on the miss path; cache per session keyed
        // on highest_seq if it shows up hot on long/active sessions.
        let touched = || {
            let mut events = Vec::new();
            let mut since = 0u64;
            loop {
                let page = store.replay_page(&session_id, since, Some(1000));
                let advance = page.last_scanned_seq;
                events.extend(page.events);
                match (page.has_more, advance) {
                    (true, Some(seq)) => since = seq,
                    _ => break,
                }
            }
            crate::server::api::file_provenance::collect_touched_paths(&events)
        };

        let confined = crate::server::api::file_provenance::confine_path(
            &roots,
            touched,
            std::path::Path::new(&requested),
        )?;
        let (content, is_binary, truncated) =
            crate::server::api::file_provenance::read_confined(&confined, MAX_CONTENTS_BYTES)?;
        Ok::<_, (StatusCode, &'static str)>(SessionFileResponse {
            content,
            is_binary,
            truncated,
        })
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err((status, msg))) => (
            status,
            Json(serde_json::json!({"error": "file_read", "message": msg})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "session_file panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct VolumeIgnoresPreviewQuery {
    pub path: String,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Serialize)]
pub struct VolumeIgnoresGlobPreview {
    pub pattern: String,
    pub matched_paths: Vec<String>,
}

#[derive(Serialize)]
pub struct VolumeIgnoresPreviewResponse {
    /// True once the user has acknowledged the snapshot-expansion behavior, so
    /// the wizard can skip the confirm modal without another round trip.
    pub acknowledged: bool,
    /// One entry per glob `volume_ignores` pattern with the directories it
    /// currently matches (container-side paths). Empty when none are configured.
    pub globs: Vec<VolumeIgnoresGlobPreview>,
}

/// Dry-run how glob `volume_ignores` entries would expand for a session rooted at
/// `path`, without creating anything. The wizard calls this before a sandbox
/// create to decide whether to show the snapshot-expansion confirm modal (#2045).
/// Read-only: no `read_only` guard needed. Closed in CityHall mode: it
/// resolves repo config for a caller-supplied host path.
pub async fn preview_volume_ignores_globs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<VolumeIgnoresPreviewQuery>,
) -> impl IntoResponse {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        let profile = query.profile.unwrap_or_default();
        let config = crate::session::config::repo_config::resolve_config_with_repo(
            &profile,
            std::path::Path::new(&query.path),
        )?;
        let expansions = crate::session::config::container_config::preview_glob_volume_ignores(
            &query.path,
            None,
            &config.sandbox.volume_ignores,
        )?;
        let acknowledged = crate::session::Config::load()
            .map(|c| c.app_state.has_acknowledged_volume_ignores_globs)
            .unwrap_or(false);
        Ok::<_, anyhow::Error>((acknowledged, expansions))
    })
    .await;

    match result {
        Ok(Ok((acknowledged, expansions))) => {
            let globs = expansions
                .into_iter()
                .map(|e| VolumeIgnoresGlobPreview {
                    pattern: e.pattern,
                    matched_paths: e.matched_container_paths,
                })
                .collect();
            (
                StatusCode::OK,
                Json(VolumeIgnoresPreviewResponse {
                    acknowledged,
                    globs,
                }),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sessions", "volume_ignores glob preview failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "preview_failed", "message": "Failed to preview volume_ignores"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "volume_ignores glob preview panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

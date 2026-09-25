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
    /// Workspace repo this file belongs to, `None` for single-repo sessions.
    /// The frontend groups the sidebar list by it and uses it to disambiguate
    /// path collisions across repos (#1047).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
}

#[derive(Serialize)]
pub struct RepoBase {
    /// None for single-repo sessions; Some for each workspace member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub base_branch: String,
    /// Worktree path this entry's diff was computed in. The web base picker
    /// queries it so a workspace member's typeahead lists its own branches
    /// rather than the launch repo's (#3329).
    pub repo_path: String,
    /// This entry's explicit override, when set. Absent means `base_branch`
    /// came from the creation base, the profile default, or auto-detection, so
    /// the client hides its reset affordance (#3329).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_override: Option<String>,
}

#[derive(Serialize)]
pub struct RichDiffFilesResponse {
    pub files: Vec<RichDiffFileInfo>,
    /// One entry per repo whose diff was computed: one element with
    /// `repo_name: None` for single-repo sessions, one per member for workspace
    /// sessions, since each member can have a different default (#1047).
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
    /// Server-computed unified diff of old to new. The client parses it as
    /// text rather than re-diffing, which would block the main thread on large
    /// files. Empty for binary files.
    pub patch: String,
    pub is_binary: bool,
    /// True if the file was too large to send inline; contents are omitted.
    pub truncated: bool,
}

/// Caps for the contents-based diff endpoint. The client renders with a
/// virtualized, off-main-thread highlighter, so the real cost is JSON payload
/// size: the byte cap guards against pathological payloads and the line cap is
/// a backstop.
const MAX_CONTENTS_BYTES: usize = 5_000_000;
const MAX_CONTENTS_LINES: usize = 200_000;

/// Validate a user-supplied relative file path against a workdir.
///
/// Returns `(canonical_path, is_changed)` when the path is safe to read (not
/// absolute, no `..`, no symlink escape). `is_changed` marks a diffable path;
/// false marks an in-repo file with no diff against the base, served via the
/// full-file fallback (#1810).
///
/// A path neither in the changed set nor on disk yields `NOT_FOUND`. The
/// non-canonical fallback is reserved for the changed-set case, where a file
/// deleted in the working tree is still diffable.
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

    // Canonicalize both sides and verify containment, as defense in depth
    // against symlinks pointing outside the workdir.
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
        // Not on disk. A changed file may have been deleted in the working
        // tree but is still diffable, so fall back to the component-vetted
        // path; an unchanged path that is not on disk has nothing to show.
        Err(_) if is_changed => Ok((full, true)),
        Err(_) => Err((StatusCode::NOT_FOUND, "file not found")),
    }
}

/// One repo's worth of diff context: a name for workspace members, the path
/// the diff helper walks, and the two base-branch layers that vary per repo.
#[derive(Clone, Debug)]
pub(super) struct DiffRepo {
    /// Workspace member name, or None for single-repo sessions.
    pub(super) name: Option<String>,
    pub(super) path: String,
    /// Explicit override for this entry's diff base, set via
    /// `PATCH /api/sessions/{id}/diff-base`, `aoe session set-base`, or the TUI
    /// diff view's `b` keybind (#970, #3329).
    pub(super) base_override: Option<String>,
    /// The branch this entry's worktree was created from. Slots below the
    /// explicit override but above the profile default and auto-detection
    /// (#1951, #3329).
    pub(super) recorded_base: Option<String>,
}

struct DiffContext {
    repos: Vec<DiffRepo>,
}

/// Expand a session into the repos whose diffs the sidebar cares about:
/// one entry per `workspace_info.repos` member, or a one-element
/// `[project_path]` list for a single-repo session (#1047).
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

/// Pick the repo a per-file request names. An omitted `?repo=` defaults to the
/// first member, matching the legacy single-repo URL contract. A named repo that
/// does not exist is rejected, so a stale link cannot quietly read the wrong one
/// (#1047).
fn select_diff_repo(
    ctx: DiffContext,
    repo: Option<&str>,
) -> Result<DiffRepo, axum::response::Response> {
    let selected = match repo {
        Some(name) => ctx
            .repos
            .into_iter()
            .find(|r| r.name.as_deref() == Some(name))
            .ok_or("unknown workspace repo"),
        // A workspace row can persist with `repos: []`.
        None => ctx.repos.into_iter().next().ok_or("workspace has no repos"),
    };
    selected.map_err(|msg| api_error(StatusCode::BAD_REQUEST, "bad_request", msg))
}

/// The repo entries for one session, split out of [`resolve_diff_repos`] so the
/// per-repo base plumbing is testable without an `AppState`.
pub(super) fn diff_repos_of(inst: &crate::session::Instance) -> Vec<DiffRepo> {
    // A session with any repo record lists one entry per repo; a session with
    // none falls back to its project_path.
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
        // A session with no repo records is single-repo, so the session-level
        // override is that entry's override. `attach_project` converts a session
        // into a workspace, so a named entry and this one never coexist (#3329).
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

/// Resolve the diff base for one repo: the repo's own override, then the base
/// its worktree was recorded as forked from, then the profile's
/// `DiffConfig.default_branch`, then auto-detection. Every layer above the
/// config default is per repo (#970, #1951, #3329).
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
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(Deserialize)]
pub struct FileDiffQuery {
    pub path: String,
    /// Workspace repo name, omitted for single-repo sessions. A workspace
    /// session that omits it defaults to the first member, so the legacy
    /// single-repo URL keeps working (#1047).
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
    let selected_repo = match resolve_diff_repos(&state, &id)
        .await
        .and_then(|ctx| select_diff_repo(ctx, query.repo.as_deref()))
    {
        Ok(r) => r,
        Err(resp) => return resp,
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

            // Files in the changed set are diffed; an in-repo file with no
            // diff is served through the full-file fallback below. The
            // traversal and containment checks are the security boundary.
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
            // base renders its current contents instead of a dead end (#1810).
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
            // patch, which it renders virtualized and off-main-thread without
            // re-running the diff algorithm.
            let contents = diff::compute_file_contents(repo_path, file_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            // additions/deletions are not computed on this path; reuse the
            // counts the changed-files scan already produced.
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
        Ok(Err(DiffFileError::BadRequest(msg))) => {
            api_error(StatusCode::BAD_REQUEST, "bad_request", msg)
        }
        Ok(Err(DiffFileError::NotFound(msg))) => api_error(StatusCode::NOT_FOUND, "not_found", msg),
        Ok(Err(DiffFileError::Internal(e))) => {
            tracing::error!(target: "http.api.sessions", "File diff failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "diff_failed",
                "Failed to compute file diff",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "File diff panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Serve the current worktree bytes of a diffed file for the dashboard's
/// "Open file". The path is confined to the selected repo's worktree with the
/// session file reader's checks and no provenance fallback, so a file deleted
/// from the worktree is a 404.
pub async fn session_diff_file_raw(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<FileDiffQuery>,
) -> impl IntoResponse {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let repo = match resolve_diff_repos(&state, &id)
        .await
        .and_then(|ctx| select_diff_repo(ctx, query.repo.as_deref()))
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let result = tokio::task::spawn_blocking(move || {
        let requested = std::path::Path::new(&query.path);
        // Diff paths are repo-relative, and refusing the rest keeps this from
        // probing host paths outside the worktree.
        if requested.is_absolute() {
            return Err((StatusCode::BAD_REQUEST, "absolute path not allowed"));
        }
        let root = std::path::Path::new(&repo.path)
            .canonicalize()
            .map_err(|_| (StatusCode::NOT_FOUND, "file not found"))?;
        let confined = crate::server::api::file_provenance::confine_path(
            std::slice::from_ref(&root),
            std::collections::HashSet::new,
            requested,
        )?;
        let bytes = crate::server::api::file_provenance::read_confined_bytes(
            &confined,
            super::artifacts::MAX_RAW_FILE_BYTES,
        )?;
        Ok((open_file_mime(requested, &bytes), bytes))
    })
    .await;

    match result {
        Ok(Ok((mime, bytes))) => {
            super::artifacts::raw_file_response(&mime, !renders_inline(&mime), "no-store", bytes)
        }
        Ok(Err((status, msg))) => (
            status,
            Json(serde_json::json!({"error": "file_read", "message": msg})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Raw file read panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// The type "Open file" serves a worktree file as. Text goes out as UTF-8 plain
/// text, since `mime_guess` misnames many source types (`.ts` is a video type)
/// and every browser shows plain text. A PDF can be plain ASCII yet still needs
/// the viewer, and a scriptable type keeps its type so it is downloaded.
fn open_file_mime(path: &std::path::Path, bytes: &[u8]) -> mime_guess::Mime {
    use mime_guess::mime;
    let guessed = mime_guess::from_path(path).first_or_octet_stream();
    let is_text = !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok();
    if is_text
        && guessed != mime::APPLICATION_PDF
        && !super::artifacts::is_scriptable(guessed.essence_str())
    {
        mime::TEXT_PLAIN_UTF_8
    } else {
        guessed
    }
}

/// Types a browser renders in a tab. Anything else is sent as an attachment,
/// which the dashboard saves under the file's own name.
fn renders_inline(ty: &mime_guess::Mime) -> bool {
    use mime_guess::mime;
    let top = ty.type_();
    top == mime::IMAGE
        || top == mime::AUDIO
        || top == mime::VIDEO
        || ty.essence_str() == "text/plain"
        || *ty == mime::APPLICATION_PDF
}

#[derive(Deserialize)]
pub struct SessionFileQuery {
    pub path: String,
}

/// Response for the session file-read endpoint, mirroring
/// [`RichFileContentsResponse`]. `content` is empty for a binary or truncated
/// file, and the client renders a notice.
#[derive(Serialize)]
pub struct SessionFileResponse {
    pub content: String,
    pub is_binary: bool,
    pub truncated: bool,
}

/// Read a session file for the dashboard file viewer (#3088).
///
/// Git-agnostic, so it works on non-git scratch sessions. A read is allowed when
/// the canonical target is under a session project root or is a path the agent
/// touched this session. Confinement and bounded reading live in
/// `file_provenance`.
pub async fn session_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SessionFileQuery>,
) -> impl IntoResponse {
    // Reads workspace file contents, the same inspection surface as the diff
    // reads, and the Files pane is hidden in CityHall.
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
        // is dropped, so a stale worktree cannot break or widen confinement.
        let roots: Vec<std::path::PathBuf> = project_paths
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();

        // Provenance fallback, deferred behind a closure so the whole session
        // log is only paged when the target is outside every project root.
        // ponytail: per-request scan on the miss path; cache per session keyed
        // on highest_seq if it shows up hot.
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
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
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
    /// True once the user has acknowledged snapshot expansion, so the wizard
    /// can skip the confirm modal without another round trip.
    pub acknowledged: bool,
    /// One entry per glob `volume_ignores` pattern with the directories it
    /// currently matches (container-side paths). Empty when none are configured.
    pub globs: Vec<VolumeIgnoresGlobPreview>,
}

/// Dry-run how glob `volume_ignores` entries would expand for a session rooted
/// at `path`, creating nothing. The wizard calls it before a sandbox create to
/// decide whether to show the confirm modal (#2045). Read-only, so no
/// `read_only` guard; closed in CityHall, since it resolves repo config for a
/// caller-supplied host path.
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
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "preview_failed",
                "Failed to preview volume_ignores",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "volume_ignores glob preview panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

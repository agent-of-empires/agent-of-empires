//! Session ID capture logic for all supported agent types.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use uuid::Uuid;

/// Iterate directory entries, silently skipping unreadable ones.
///
/// Wraps `std::fs::read_dir` and filters out individual entry errors (e.g.
/// broken symlinks, transient permission failures) so that one bad entry
/// doesn't abort the entire directory scan.
pub(crate) fn resilient_read_dir(
    dir: &std::path::Path,
) -> Result<impl Iterator<Item = std::fs::DirEntry> + '_> {
    Ok(std::fs::read_dir(dir)?.filter_map(move |entry| {
        entry
            .map_err(|e| tracing::debug!(target: "session.capture", "Skipping unreadable entry in {}: {}", dir.display(), e))
            .ok()
    }))
}

/// Resolve an agent's home directory, checking an optional env var first.
fn resolve_agent_home(env_var: Option<&str>, default_subdir: &str) -> Result<PathBuf> {
    if let Some(var) = env_var {
        if let Ok(val) = std::env::var(var) {
            return Ok(PathBuf::from(val));
        }
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(default_subdir))
}

fn canonicalize_or_raw(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Validate a captured session ID, logging a warning if it fails.
///
/// Single checkpoint at the capture boundary so that invalid IDs never
/// propagate into storage.
pub(crate) fn validated_session_id(id: String) -> Option<String> {
    if is_valid_session_id(&id) {
        Some(id)
    } else {
        tracing::warn!(target: "session.capture", "Captured session ID failed validation: {:?}", id);
        None
    }
}

/// Generate a new UUID v4 for a Claude Code session.
pub(crate) fn generate_claude_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Encode a project path into Claude Code's directory naming convention.
///
/// Claude stores per-project data under `~/.claude/projects/{encoded}/` where
/// non-alphanumeric characters (except `-`) are replaced with `-`.
/// For example: `/Users/foo/bar` becomes `-Users-foo-bar`.
pub(crate) fn encode_claude_project_path(project_path: &str) -> String {
    project_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Capture Claude Code session ID from the most recently active project directory,
/// falling back to `~/.claude.json` if the dir scan result is stale.
///
/// Used as a fallback when hooks don't fire (e.g. after `/clear` or `/new`).
pub(crate) fn capture_claude_session_id(
    project_path: &str,
    known_session_id: Option<&str>,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let claude_home = resolve_agent_home(Some("CLAUDE_CONFIG_DIR"), ".claude")?;
    let canonical = canonicalize_or_raw(project_path);

    if let Some((id, modified)) =
        scan_claude_project_dir(&claude_home, &canonical, known_session_id, exclusion)?
    {
        let age = modified.elapsed().unwrap_or(Duration::from_secs(u64::MAX));
        if age <= Duration::from_secs(5 * 60) {
            return Ok(id);
        }
    }

    if let Some(id) = read_claude_json_session_id(&canonical) {
        if exclusion.contains(&id) {
            return Err(anyhow::anyhow!(
                "claude.json lastSessionId {} is excluded (claimed by another instance)",
                id
            ));
        }
        let claude_json = dirs::home_dir()
            .map(|h| h.join(".claude.json"))
            .and_then(|p| std::fs::metadata(&p).ok())
            .and_then(|m| m.modified().ok());
        let is_fresh = claude_json
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age <= Duration::from_secs(5 * 60));
        if is_fresh && Uuid::parse_str(&id).is_ok() {
            return Ok(id);
        }
    }

    anyhow::bail!("No active Claude session found for {}", project_path)
}

/// Scan `~/.claude/projects/{encoded-path}/` and pick this poller's session.
///
/// Tie-break:
/// 1. anchor stale or absent → return `best` (most-recent unexcluded jsonl).
/// 2. `best` exists, fresh, and strictly newer than the anchor → return
///    `best`. The caller promotes `last_known` so the poller adopts the new
///    UUID after `/clear` / `/new` / `--fork-session` mints a new jsonl.
/// 3. otherwise → return the anchor (covers steady-state and the case where
///    a sibling's most-recent write was filtered out by `exclusion`).
fn scan_claude_project_dir(
    claude_home: &Path,
    project_path: &Path,
    known: Option<&str>,
    exclusion: &HashSet<String>,
) -> Result<Option<(String, std::time::SystemTime)>> {
    let dir_name = encode_claude_project_path(&project_path.to_string_lossy());
    let project_dir = claude_home.join("projects").join(&dir_name);

    if !project_dir.is_dir() {
        return Ok(None);
    }

    let mut best: Option<(String, std::time::SystemTime)> = None;
    let mut known_hit: Option<(String, std::time::SystemTime)> = None;

    for entry in resilient_read_dir(&project_dir)? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if Uuid::parse_str(stem).is_err() {
            continue;
        }

        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        if known == Some(stem) && !exclusion.contains(stem) {
            known_hit = Some((stem.to_string(), modified));
        }

        if exclusion.contains(stem) {
            continue;
        }

        if best.as_ref().is_none_or(|(_, t)| modified > *t) {
            best = Some((stem.to_string(), modified));
        }
    }

    if let Some((kid, kmt)) = known_hit {
        let known_fresh = kmt
            .elapsed()
            .map(|age| age <= Duration::from_secs(5 * 60))
            .unwrap_or(false);
        if !known_fresh {
            return Ok(best);
        }
        if let Some((_, bmt)) = best.as_ref() {
            let best_fresh = bmt
                .elapsed()
                .map(|age| age <= Duration::from_secs(5 * 60))
                .unwrap_or(false);
            if best_fresh && *bmt > kmt {
                return Ok(best);
            }
        }
        return Ok(Some((kid, kmt)));
    }

    Ok(best)
}

/// Read `lastSessionId` from `~/.claude.json` for a given project path.
fn read_claude_json_session_id(project_path: &Path) -> Option<String> {
    let claude_json = dirs::home_dir()?.join(".claude.json");
    let content = std::fs::read_to_string(&claude_json).ok()?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;

    let path_str = project_path.to_string_lossy();
    parsed
        .get("projects")?
        .get(path_str.as_ref())?
        .get("lastSessionId")?
        .as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Polling closure for Claude Code session tracking on the host filesystem.
///
/// Per tick, in order:
/// 1. Read `/tmp/aoe-hooks-<euid>/<instance_id>/session_id` (written by Claude's
///    `SessionStart` / `UserPromptSubmit` hooks). When present and ≤ 5 min
///    old, return it and skip the disk scan.
/// 2. Otherwise scan `~/.claude/projects/<encoded-path>/`. The scan uses
///    `compose_exclusion(instance_id, extra_excludes)` to skip UUIDs claimed
///    by peers via tmux env, and the `last_known` mutex to anchor this
///    closure to its own session even when a peer's jsonl is more recent.
///    Each successful capture promotes `last_known` so subsequent ticks see
///    the new anchor.
pub(crate) fn claude_poll_fn(
    project_path: String,
    known_session_id: Option<String>,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let last_known = std::sync::Mutex::new(known_session_id);
    move || {
        // Sidecar reads are scoped per-instance: the file lives under
        // `/tmp/aoe-hooks-<euid>/<instance_id>/` so a sibling instance's hook
        // writes cannot reach this path, which is why the read skips
        // `compose_exclusion`. `extra_excludes` is still honored so a
        // sidecar value matching one of this instance's cleared sids does
        // not leak through.
        if let Some(id) = crate::hooks::read_hook_session_id(&instance_id) {
            if !extra_excludes.contains(&id) {
                if let Some(validated) = validated_session_id(id) {
                    if let Ok(mut guard) = last_known.lock() {
                        *guard = Some(validated.clone());
                    }
                    return Some(validated);
                }
            }
        }

        let current_known = last_known.lock().ok().and_then(|g| g.clone());
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        let captured = capture_claude_session_id(
            &project_path,
            current_known.as_deref(),
            &exclusion,
        )
        .map_err(|e| tracing::debug!(target: "session.capture", "Claude disk scan failed: {}", e))
        .ok()
        .and_then(validated_session_id);

        if let Some(id) = captured.as_ref() {
            if let Ok(mut guard) = last_known.lock() {
                *guard = Some(id.clone());
            }
        }

        captured
    }
}

/// Capture Claude Code session ID inside a Docker container.
///
/// Lists every fresh (≤ 5 min mtime) UUID-named jsonl in
/// `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/` newest-first via
/// `docker exec`, wrapped in [`run_with_timeout`] (5 s) so a hung exec
/// cannot block the poller thread, then delegates per-pane attribution to
/// [`select_claude_session_in_container`].
pub(crate) fn capture_claude_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
    known_session_id: Option<&str>,
) -> Result<String> {
    let dir_name = encode_claude_project_path(container_cwd);

    let snippet = format!(
        r#"
CLAUDE_HOME="${{CLAUDE_CONFIG_DIR:-$HOME/.claude}}"
DIR="$CLAUDE_HOME/projects/{dir_name}"
[ -d "$DIR" ] || exit 0
for f in $(ls -t "$DIR"/*.jsonl 2>/dev/null); do
  [ -n "$(find "$f" -mmin -5 2>/dev/null)" ] || continue
  basename "$f" .jsonl
done
"#
    );

    let mut cmd = std::process::Command::new("docker");
    cmd.args(["exec", container_name, "sh", "-c", &snippet]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(5),
        "docker exec sh (claude jsonl scan)",
    )
    .map_err(|e| anyhow::anyhow!("{} (container {})", e, container_name))?;

    select_claude_session_in_container(&stdout_bytes, exclusion, known_session_id)
        .map_err(|e| anyhow::anyhow!("{} (container {})", e, container_name))
}

/// Pick the active Claude session UUID from the container shell snippet's
/// stdout.
///
/// Stdout is UUID basenames in newest-first order. Tie-break (mirrors
/// [`scan_claude_project_dir`]):
/// 1. anchor absent → return first unexcluded.
/// 2. anchor present, an unexcluded candidate appears before it → return
///    that candidate (active newer wins).
/// 3. otherwise → return the anchor.
fn select_claude_session_in_container(
    stdout_bytes: &[u8],
    exclusion: &HashSet<String>,
    known: Option<&str>,
) -> Result<String> {
    let text = String::from_utf8_lossy(stdout_bytes);
    let candidates: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && Uuid::parse_str(l).is_ok())
        .map(String::from)
        .collect();

    if candidates.is_empty() {
        anyhow::bail!("No active Claude session found in container");
    }

    let known_pos = known.and_then(|k| {
        if exclusion.contains(k) {
            None
        } else {
            candidates.iter().position(|c| c == k)
        }
    });
    let best_pos = candidates.iter().position(|c| !exclusion.contains(c));

    match (known_pos, best_pos) {
        (None, None) => {
            anyhow::bail!("All Claude session candidates in container are excluded")
        }
        (None, Some(p)) => Ok(candidates[p].clone()),
        (Some(kp), Some(bp)) if bp < kp => Ok(candidates[bp].clone()),
        (Some(kp), _) => Ok(candidates[kp].clone()),
    }
}

/// Polling closure for sandboxed (Docker) Claude Code session tracking.
///
/// Mirrors [`claude_poll_fn`] but does not read the host hook sidecar (the
/// in-container hook would write to the container's `/tmp/aoe-hooks/`,
/// which the host poller cannot see without bind-mounting). Sandboxed
/// `/clear` adoption therefore takes ≤ 1 poll tick.
pub(crate) fn claude_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    known_session_id: Option<String>,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let last_known = std::sync::Mutex::new(known_session_id);
    move || {
        let current_known = last_known.lock().ok().and_then(|g| g.clone());
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        let captured = capture_claude_session_id_in_container(
            &container_name,
            &container_cwd,
            &exclusion,
            current_known.as_deref(),
        )
        .map_err(
            |e| tracing::debug!(target: "session.capture", "Claude container scan failed: {}", e),
        )
        .ok()
        .and_then(validated_session_id);

        if let Some(id) = captured.as_ref() {
            if let Ok(mut guard) = last_known.lock() {
                *guard = Some(id.clone());
            }
        }

        captured
    }
}

pub(crate) fn encode_pi_project_path(cwd: &str) -> String {
    let stripped = cwd
        .strip_prefix('/')
        .or_else(|| cwd.strip_prefix('\\'))
        .unwrap_or(cwd);

    let encoded: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            _ => c,
        })
        .collect();

    format!("--{encoded}--")
}

fn extract_pi_header_fields(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let first_line = std::io::BufRead::lines(reader).next()?.ok()?;
    parse_pi_header_json(&first_line)
}

/// Parse the first line of a Pi `.jsonl` session file (already in memory).
///
/// Shared by the host scanner and the container scanner, which receives
/// header lines via `docker exec` rather than direct filesystem reads.
fn parse_pi_header_json(line: &str) -> Option<(Option<String>, Option<String>)> {
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    if parsed.get("type")?.as_str()? != "session" {
        return None;
    }
    let session_id = parsed.get("id").and_then(|v| v.as_str()).map(String::from);
    let cwd = parsed
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    Some((session_id, cwd))
}

pub(crate) fn extract_pi_session_id_from_header(path: &Path) -> Option<String> {
    extract_pi_header_fields(path).and_then(|(id, _)| id)
}

#[cfg(test)]
pub(crate) fn extract_pi_cwd_from_header(path: &Path) -> Option<String> {
    extract_pi_header_fields(path).and_then(|(_, cwd)| cwd)
}

pub(crate) fn extract_pi_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let uuid_part = stem.rsplit('_').next()?;
    Uuid::parse_str(uuid_part).ok()?;
    Some(uuid_part.to_string())
}

/// Capture Pi session ID by scanning the Pi agent sessions directory.
///
/// Looks for `.jsonl` session files under `~/.pi/agent/sessions/` (or
/// `$PI_CODING_AGENT_DIR/sessions/`). The primary lookup uses the encoded
/// project path as a directory name. Falls back to scanning all session
/// directories and matching via the `cwd` header field.
pub(crate) fn capture_pi_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let pi_home = resolve_agent_home(Some("PI_CODING_AGENT_DIR"), ".pi/agent")?;
    let sessions_dir = pi_home.join("sessions");

    if !sessions_dir.exists() {
        anyhow::bail!(
            "Pi sessions directory not found: {}",
            sessions_dir.display()
        );
    }

    let encoded_name = encode_pi_project_path(project_path);
    let project_dir = sessions_dir.join(&encoded_name);

    if project_dir.is_dir() {
        let mut candidates: Vec<(String, std::time::SystemTime)> = Vec::new();

        for entry in resilient_read_dir(&project_dir)? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = match extract_pi_session_id_from_header(&path) {
                Some(id) if !id.is_empty() && !exclusion.contains(&id) => id,
                _ => continue,
            };
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((session_id, modified));
        }

        candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

        if let Some((id, _)) = candidates.first() {
            return Ok(id.clone());
        }
    }

    // Fallback: scan all subdirectories and match via CWD header
    let canonical_project = canonicalize_or_raw(project_path);
    let mut fallback_candidates: Vec<(String, std::time::SystemTime)> = Vec::new();

    for subdir_entry in resilient_read_dir(&sessions_dir)? {
        let subdir_path = subdir_entry.path();
        if !subdir_path.is_dir() {
            continue;
        }
        for file_entry in resilient_read_dir(&subdir_path)? {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let fields = match extract_pi_header_fields(&file_path) {
                Some(f) => f,
                None => continue,
            };
            let cwd = match fields.1 {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };
            let canonical_cwd = canonicalize_or_raw(&cwd);
            if canonical_cwd != canonical_project {
                continue;
            }
            let session_id = match fields.0 {
                Some(id) if !id.is_empty() && !exclusion.contains(&id) => id,
                _ => continue,
            };
            let modified = file_entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            fallback_candidates.push((session_id, modified));
        }
    }

    fallback_candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

    if let Some((id, _)) = fallback_candidates.first() {
        return Ok(id.clone());
    }

    // Third fallback: when all JSONL headers fail to parse, pick the most
    // recently modified session directory and extract a UUID from its files.
    let mut dirs_by_mtime: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = resilient_read_dir(&sessions_dir) {
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            dirs_by_mtime.push((path, mtime));
        }
    }
    dirs_by_mtime.sort_by_key(|c| std::cmp::Reverse(c.1));

    for (dir, _) in &dirs_by_mtime {
        if let Ok(entries) = resilient_read_dir(dir) {
            let mut file_candidates: Vec<(String, std::time::SystemTime)> = Vec::new();
            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Some(uuid) = extract_pi_uuid_from_filename(&path) {
                    if !exclusion.contains(&uuid) {
                        let mtime = entry
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        file_candidates.push((uuid, mtime));
                    }
                }
            }
            file_candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
            if let Some((id, _)) = file_candidates.first() {
                return Ok(id.clone());
            }
        }
    }

    anyhow::bail!("No Pi session found matching project path")
}

pub(crate) fn pi_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_pi_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Pi poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

const PI_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Shell snippet executed via `docker exec` to enumerate Pi `.jsonl` session
/// files inside the container. Each file is emitted as a `===PI:<unix-mtime>===`
/// header followed by the first line of the file (the session header) and a
/// `===END===` trailer; the host parses this stream rather than spawning one
/// `docker exec head` per file.
const PI_CONTAINER_LIST_SCRIPT: &str = r#"SESS_DIR="${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/sessions"
[ -d "$SESS_DIR" ] || exit 0
for d in "$SESS_DIR"/*/; do
  for f in "$d"*.jsonl; do
    [ -f "$f" ] || continue
    ts=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)
    printf '===PI:%s===\n' "$ts"
    head -n 1 "$f"
    printf '\n===END===\n'
  done
done
"#;

/// Capture a Pi session ID from inside a Docker container.
///
/// Mirrors `capture_pi_session_id` but reads `.jsonl` headers via
/// `docker exec sh` since pi-in-container writes to the container's
/// `~/.pi/agent/sessions/`. Matches against `container_cwd` (the path
/// pi-in-container records), not the host project path.
pub(crate) fn try_capture_pi_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["exec", container_name, "sh", "-c", PI_CONTAINER_LIST_SCRIPT]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(PI_COMMAND_TIMEOUT_SECS),
        "docker exec sh (pi session scan)",
    )?;
    select_pi_session_in_container(&stdout_bytes, container_cwd, exclusion)
}

/// Parse the delimited stream emitted by `PI_CONTAINER_LIST_SCRIPT` and pick
/// the most recent session whose recorded CWD matches `container_cwd`.
fn select_pi_session_in_container(
    stdout_bytes: &[u8],
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = String::from_utf8_lossy(stdout_bytes);
    let mut candidates: Vec<(String, Option<String>, u64)> = Vec::new();

    for chunk in text.split("===PI:").skip(1) {
        let (ts_str, rest) = match chunk.split_once("===\n") {
            Some(p) => p,
            None => continue,
        };
        let ts: u64 = ts_str.trim().parse().unwrap_or(0);
        let json_part = match rest.split_once("\n===END===") {
            Some((j, _)) => j,
            None => rest,
        };
        let (id_opt, cwd) = match parse_pi_header_json(json_part.trim()) {
            Some(p) => p,
            None => continue,
        };
        let session_id = match id_opt {
            Some(id) if !id.is_empty() && !exclusion.contains(&id) => id,
            _ => continue,
        };
        candidates.push((session_id, cwd, ts));
    }

    if candidates.is_empty() {
        anyhow::bail!("No Pi sessions found in container");
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));

    let project_match = candidates
        .iter()
        .find(|(_, cwd, _)| cwd.as_deref() == Some(container_cwd));

    project_match
        .map(|(id, _, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No Pi session matching container CWD"))
}

/// Polling closure for sandboxed (Docker) Pi session tracking.
pub(crate) fn pi_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_pi_session_id_in_container(&container_name, &container_cwd, &exclusion)
            .map_err(|e| tracing::debug!(target: "session.capture", "Pi container poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

pub(crate) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Compose [`build_exclusion_set`] (cross-instance live tmux scan) with a
/// per-instance set of IDs the cascade has explicitly cleared but which
/// may still live on disk for several minutes.
///
/// Both `Instance::retroactive_capture_exclusion_set` and the post-launch
/// `*_poll_fn` closures route through this helper so the resume-fallback
/// cascade's just-crashed sid is filtered identically on the synchronous
/// pre-launch path and on the asynchronous polling path.
pub(crate) fn compose_exclusion(
    current_instance_id: &str,
    extra: &HashSet<String>,
) -> HashSet<String> {
    let mut set = build_exclusion_set(current_instance_id);
    set.extend(extra.iter().cloned());
    set
}

/// Extend [`compose_exclusion`] with the sids of stopped, archived, or
/// pane-less Claude peers in the same `project_path`, read from
/// `sessions.json` via `Storage` for the caller's effective profile.
///
/// Used only by the Claude branch of `Instance::try_retroactive_capture`
/// when the per-instance sidecar is absent or stale: the mtime fallback
/// over `~/.claude/projects/<encoded-cwd>/` otherwise picks a co-located
/// stopped peer's jsonl, since [`build_exclusion_set`] only sees live
/// tmux peers, and the resume binds to the peer's conversation (#2355).
///
/// `claude_poll_fn` keeps the live-only exclusion via [`compose_exclusion`]:
/// it runs on a hot polling path, and live peers already surface in the
/// tmux env scan.
///
/// Scope: `~/.claude/projects/` is keyed by `$HOME`, not by AoE profile,
/// but this helper only inspects `sessions.json` for the caller's effective
/// profile. A stopped peer in a different profile against the same host
/// `$HOME` will not be excluded; the residual gap is narrower than #2355's
/// trigger and is left for a follow-up.
pub(crate) fn compose_exclusion_with_stopped_peers(
    current_instance_id: &str,
    current_project_path: &str,
    profile: &str,
    retroactive_capture_excludes: &HashSet<String>,
) -> HashSet<String> {
    let mut set = compose_exclusion(current_instance_id, retroactive_capture_excludes);
    let Ok(storage) = crate::session::storage::Storage::new_unwatched(profile) else {
        return set;
    };
    let Ok(instances) = storage.load() else {
        return set;
    };
    for inst in instances {
        if inst.id == current_instance_id {
            continue;
        }
        if inst.tool != "claude" {
            continue;
        }
        if inst.project_path != current_project_path {
            continue;
        }
        let should_exclude = matches!(inst.status, crate::session::Status::Stopped)
            || inst.is_archived()
            || !inst.has_live_tmux_pane();
        if !should_exclude {
            continue;
        }
        if let Some(sid) = inst.agent_session_id.as_deref().filter(|s| !s.is_empty()) {
            set.insert(sid.to_string());
        }
    }
    set
}

/// Build the set of session IDs already claimed by other live AoE instances.
///
/// Reads every other live AoE tmux session's hidden env to find which session
/// IDs are currently bound to which instance, and returns the set of captured
/// IDs that belong to instances OTHER than `current_instance_id`.
/// Used by post-launch poll closures to avoid re-importing another
/// instance's session via filesystem scan.
///
/// Callers that also need to exclude IDs not yet visible in tmux env (e.g.
/// the resume-fallback cascade's just-crashed sid) should use
/// [`compose_exclusion`] instead, which composes this function with the
/// per-instance exclusion list.
fn build_exclusion_set(current_instance_id: &str) -> HashSet<String> {
    let output = match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return HashSet::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let aoe_sessions: Vec<&str> = stdout
        .lines()
        .filter(|name| {
            name.starts_with(crate::tmux::SESSION_PREFIX)
                && !name.starts_with(crate::tmux::TOOL_PREFIX)
        })
        .collect();

    if aoe_sessions.is_empty() {
        return HashSet::new();
    }

    let instance_ids = crate::tmux::env::get_hidden_env_batch(
        &aoe_sessions,
        crate::tmux::env::AOE_INSTANCE_ID_KEY,
    );

    let other_sessions: Vec<&str> = instance_ids
        .iter()
        .filter(|(_, owner)| {
            owner
                .as_deref()
                .is_some_and(|owner| owner != current_instance_id)
        })
        .map(|(name, _)| name.as_str())
        .collect();

    if other_sessions.is_empty() {
        return HashSet::new();
    }

    let captured_ids = crate::tmux::env::get_hidden_env_batch(
        &other_sessions,
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
    );

    captured_ids.into_iter().filter_map(|(_, id)| id).collect()
}

/// Capture Vibe session ID from `meta.json` files in the session log directory.
///
/// Default path: `~/.vibe/logs/session/`; overridden by `VIBE_HOME` env var
/// (resolves to `$VIBE_HOME/logs/session/`).
/// Each session dir contains `meta.json` with `session_id` and
/// `environment.working_directory`. Returns the most recent match for the project.
pub(crate) fn capture_vibe_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let vibe_home = resolve_agent_home(Some("VIBE_HOME"), ".vibe")?;
    let sessions_dir = vibe_home.join("logs").join("session");

    if !sessions_dir.exists() {
        anyhow::bail!(
            "Vibe sessions directory not found: {}",
            sessions_dir.display()
        );
    }

    let mut candidates: Vec<(String, Option<String>, std::time::SystemTime)> = Vec::new();

    for entry in resilient_read_dir(&sessions_dir)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let (session_id, cwd) = match extract_vibe_meta(&meta_path) {
            Some(pair) if !pair.0.is_empty() && !exclusion.contains(&pair.0) => pair,
            _ => continue,
        };
        let modified = std::fs::metadata(&meta_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((session_id, cwd, modified));
    }

    if candidates.is_empty() {
        anyhow::bail!(
            "No Vibe session directories found in {}",
            sessions_dir.display()
        );
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));

    let canonical_project = canonicalize_or_raw(project_path);

    let project_match = candidates.iter().find(|(_, cwd, _)| {
        cwd.as_ref()
            .and_then(|cwd| std::fs::canonicalize(cwd).ok())
            .map(|cwd| cwd == canonical_project)
            .unwrap_or(false)
    });

    project_match
        .map(|(id, _, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No Vibe session found matching project path"))
}

/// Parse a Vibe `meta.json`, returning `(session_id, working_directory)`.
///
/// Returns `None` if the file can't be read, isn't valid JSON, or lacks
/// a `session_id` string. The working directory comes from
/// `environment.working_directory`.
fn extract_vibe_meta(path: &Path) -> Option<(String, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_vibe_meta_json(&content)
}

/// Parse the body of a Vibe `meta.json` (already in memory).
///
/// Shared by the host scanner and the container scanner, which receives
/// `meta.json` contents via `docker exec` rather than direct filesystem reads.
fn parse_vibe_meta_json(content: &str) -> Option<(String, Option<String>)> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    let session_id = parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)?;
    let cwd = parsed
        .get("environment")
        .and_then(|env| env.get("working_directory"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Some((session_id, cwd))
}

/// Polling closure for Vibe (Mistral) session tracking.
pub(crate) fn vibe_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_vibe_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Vibe poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

const VIBE_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Shell snippet executed via `docker exec` to enumerate Vibe `meta.json` files
/// inside the container. Each file is emitted as a `===VIBE:<unix-mtime>===`
/// header followed by the JSON body and a `===END===` trailer; the host parses
/// this stream rather than spawning one `docker exec cat` per file.
const VIBE_CONTAINER_LIST_SCRIPT: &str = r#"SESS_DIR="${VIBE_HOME:-$HOME/.vibe}/logs/session"
[ -d "$SESS_DIR" ] || exit 0
for d in "$SESS_DIR"/*/; do
  m="$d/meta.json"
  [ -f "$m" ] || continue
  ts=$(stat -c %Y "$m" 2>/dev/null || stat -f %m "$m" 2>/dev/null || echo 0)
  printf '===VIBE:%s===\n' "$ts"
  cat "$m"
  printf '\n===END===\n'
done
"#;

/// Capture a Vibe session ID from inside a Docker container.
///
/// Mirrors `capture_vibe_session_id` but reads `meta.json` files via
/// `docker exec sh` since vibe-in-container writes to the container's
/// `~/.vibe/logs/session/`. Matches against `container_cwd` (the path
/// vibe-in-container records), not the host project path.
pub(crate) fn try_capture_vibe_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "exec",
        container_name,
        "sh",
        "-c",
        VIBE_CONTAINER_LIST_SCRIPT,
    ]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(VIBE_COMMAND_TIMEOUT_SECS),
        "docker exec sh (vibe meta scan)",
    )?;
    select_vibe_session_in_container(&stdout_bytes, container_cwd, exclusion)
}

/// Parse the delimited stream emitted by `VIBE_CONTAINER_LIST_SCRIPT` and pick
/// the most recent session whose recorded CWD matches `container_cwd`.
fn select_vibe_session_in_container(
    stdout_bytes: &[u8],
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = String::from_utf8_lossy(stdout_bytes);
    let mut candidates: Vec<(String, Option<String>, u64)> = Vec::new();

    for chunk in text.split("===VIBE:").skip(1) {
        let (ts_str, rest) = match chunk.split_once("===\n") {
            Some(p) => p,
            None => continue,
        };
        let ts: u64 = ts_str.trim().parse().unwrap_or(0);
        let json_part = match rest.split_once("\n===END===") {
            Some((j, _)) => j,
            None => rest,
        };
        let (session_id, cwd) = match parse_vibe_meta_json(json_part.trim()) {
            Some(pair) if !pair.0.is_empty() && !exclusion.contains(&pair.0) => pair,
            _ => continue,
        };
        candidates.push((session_id, cwd, ts));
    }

    if candidates.is_empty() {
        anyhow::bail!("No Vibe sessions found in container");
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));

    let project_match = candidates
        .iter()
        .find(|(_, cwd, _)| cwd.as_deref() == Some(container_cwd));

    project_match
        .map(|(id, _, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No Vibe session matching container CWD"))
}

/// Polling closure for sandboxed (Docker) Vibe session tracking.
pub(crate) fn vibe_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_vibe_session_id_in_container(&container_name, &container_cwd, &exclusion)
            .map_err(|e| tracing::debug!(target: "session.capture", "Vibe container poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

/// Filter, sort, and deduplicate agent sessions by project directory.
///
/// Given a list of parsed session JSON values:
/// 1. Filters to sessions matching `project_path` (canonicalized comparison on `directory`)
/// 2. Sorts by `updated` timestamp descending (most recent first)
/// 3. If `launch_time_ms` is `Some`, removes sessions older than that threshold
/// 4. Removes sessions whose IDs appear in `exclusion`
pub(crate) fn filter_agent_sessions<'a>(
    session_entries: &'a [serde_json::Value],
    project_path: Option<&str>,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Vec<&'a serde_json::Value> {
    let mut matching: Vec<&serde_json::Value> = if let Some(path) = project_path {
        let canonical_path = canonicalize_or_raw(path);
        let canonical_str = canonical_path.to_string_lossy();

        session_entries
            .iter()
            .filter(|s| {
                s.get("directory")
                    .and_then(|v| v.as_str())
                    .map(|dir| {
                        let session_path = canonicalize_or_raw(dir);
                        session_path.to_string_lossy() == canonical_str
                    })
                    .unwrap_or(false)
            })
            .collect()
    } else {
        session_entries.iter().collect()
    };

    matching.sort_by(|a, b| {
        let a_time = a.get("updated").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let b_time = b.get("updated").and_then(|v| v.as_f64()).unwrap_or(0.0);
        b_time
            .partial_cmp(&a_time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if let Some(threshold) = launch_time_ms {
        matching.retain(|s| s.get("updated").and_then(|v| v.as_f64()).unwrap_or(0.0) >= threshold);
    }

    matching.retain(|s| {
        s.get("id")
            .and_then(|v| v.as_str())
            .map(|id| !exclusion.contains(id))
            .unwrap_or(true)
    });

    matching
}

const OPENCODE_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Spawn `cmd`, read stdout to EOF on a worker thread, and wait for the
/// process to exit. Kills the child if `timeout` elapses first.
fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
    label: &str,
) -> Result<Vec<u8>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", label))?;

    let stdout_pipe = child.stdout.take();
    let stdout_handle = std::thread::spawn(move || {
        stdout_pipe.map(|mut r| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut r, &mut buf).ok();
            buf
        })
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!("{} timed out", label));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::anyhow!("Failed to wait on {}: {}", label, e)),
        }
    };

    let stdout_bytes = stdout_handle.join().ok().flatten().unwrap_or_default();

    if !status.success() {
        anyhow::bail!("{} command failed", label);
    }

    Ok(stdout_bytes)
}

/// Parse `opencode session list --format json` output and pick the best match.
///
/// `match_path` is the directory the session's `directory` field is compared
/// against. For host capture this is the host project path; for sandboxed
/// capture this is the container CWD (since opencode records its own CWD).
fn select_opencode_session(
    stdout_bytes: &[u8],
    match_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let stdout = String::from_utf8_lossy(stdout_bytes);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        anyhow::bail!("No OpenCode sessions found");
    }
    let session_entries: Vec<serde_json::Value> =
        serde_json::from_str(trimmed).context("Failed to parse OpenCode session list JSON")?;

    select_opencode_session_from_values(&session_entries, match_path, exclusion, launch_time_ms)
}

/// Pick the best opencode session from already-parsed entries.
///
/// Shared by the JSON (subprocess) path and the SQLite (direct read) path.
/// Each entry must expose `id`, `directory`, and `updated` keys; the SQLite
/// reader synthesizes that shape from the `session` table so this filter
/// behaves identically across both sources.
fn select_opencode_session_from_values(
    session_entries: &[serde_json::Value],
    match_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let matching =
        filter_agent_sessions(session_entries, Some(match_path), exclusion, launch_time_ms);

    matching
        .first()
        .and_then(|s| s["id"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No OpenCode sessions found matching project path"))
}

/// Resolve the path to opencode's local SQLite session store.
///
/// Mirrors opencode's own resolution order (see `packages/opencode/src/storage/db.ts`):
///   1. `OPENCODE_DB` env var: absolute path used verbatim, relative path
///      joined to data dir, `:memory:` is unsupported (bail).
///   2. Most recently modified `opencode*.db` in the data dir (covers both
///      the standard `opencode.db` and channel variants like `opencode-dev.db`).
fn opencode_db_path() -> Result<PathBuf> {
    // 1. Explicit override via OPENCODE_DB (same env var opencode reads).
    if let Ok(db_env) = std::env::var("OPENCODE_DB") {
        if !db_env.is_empty() {
            if db_env == ":memory:" {
                anyhow::bail!("opencode is using an in-memory DB; cannot read sessions via SQLite");
            }
            let p = PathBuf::from(&db_env);
            if p.is_absolute() {
                return Ok(p);
            }
            return Ok(opencode_data_dir()?.join(p));
        }
    }

    let data_dir = opencode_data_dir()?;

    // 2. Find the most recently modified opencode DB in data_dir.
    //    Covers both the standard filename (latest/beta/prod) and channel
    //    variants (opencode-{channel}.db). Picking by mtime ensures we read
    //    the DB the running opencode instance is actively writing to.
    if let Ok(entries) = std::fs::read_dir(&data_dir) {
        let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let is_candidate = name_str == "opencode.db"
                || (name_str.starts_with("opencode-") && name_str.ends_with(".db"));
            if is_candidate {
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
                        best = Some((entry.path(), mtime));
                    }
                }
            }
        }
        if let Some((path, _)) = best {
            return Ok(path);
        }
    }

    // Nothing found; return standard path so the caller gets a clear
    // "not found" error and falls back to the subprocess path.
    Ok(data_dir.join("opencode.db"))
}

/// Resolve opencode's data directory.
///
/// Uses `XDG_DATA_HOME` if set (same `xdg-basedir` npm package opencode uses),
/// otherwise `$HOME/.local/share`. Both Linux and macOS use this path.
fn opencode_data_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("opencode"));
        }
    }
    let home =
        std::env::var("HOME").context("HOME is not set; cannot resolve opencode data dir")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("opencode"))
}

/// Load opencode's session rows from its SQLite store at `db_path`.
///
/// Selects `id`, `directory`, and `time_updated` from the `session` table
/// and reshapes each row to match the JSON the CLI would emit (`id`,
/// `directory`, `updated`). An `Err` here means the DB is unreadable
/// (missing, locked, schema mismatch, IO) and the caller should fall back
/// to the subprocess path. An `Ok` with an empty `Vec` is authoritative:
/// opencode genuinely has no sessions, so we should NOT fall back; the
/// subprocess would leak `/tmp/.<hash>.so` for the same empty answer.
fn read_opencode_sessions_from_sqlite_at(db_path: &Path) -> Result<Vec<serde_json::Value>> {
    use rusqlite::{Connection, OpenFlags};

    if !db_path.exists() {
        anyhow::bail!("opencode DB not found at {}", db_path.display());
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Failed to open opencode DB at {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_millis(100))
        .context("Failed to set opencode DB busy timeout")?;

    let mut stmt = conn
        .prepare("SELECT id, directory, time_updated FROM session ORDER BY time_updated DESC")
        .context("opencode session table schema mismatch")?;

    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let directory: String = row.get(1)?;
            let time_updated: i64 = row.get(2)?;
            Ok(serde_json::json!({
                "id": id,
                "directory": directory,
                "updated": time_updated as f64,
            }))
        })
        .context("Failed to query opencode session table")?;

    let mut entries: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        entries.push(row.context("Failed to read opencode session row")?);
    }
    Ok(entries)
}

fn log_opencode_sqlite_fallback_once(err: &anyhow::Error) {
    use std::sync::Once;
    static LOG_ONCE: Once = Once::new();
    LOG_ONCE.call_once(|| {
        tracing::warn!(target: "session.capture", 
            "opencode SQLite read failed ({}); falling back to `opencode session list`. \
             That subprocess leaks /tmp/.<hash>.so files via bun:ffi (see anomalyco/opencode#6523).",
            err
        );
    });
}

/// Capture an OpenCode session ID, preferring a direct SQLite read.
///
/// `launch_time_ms` is the lower bound on the session's `updated` timestamp,
/// used to ignore stale sessions left over from prior runs. Pass `None` for
/// retroactive capture on TUI startup, when the launch time isn't known.
///
/// Reads `~/.local/share/opencode/opencode.db` (or `$XDG_DATA_HOME/opencode/`)
/// first. If the DB exists and is readable, its result is authoritative
/// (including "no match found"); we do NOT fall back to the subprocess in
/// that case, because every `opencode session list` invocation leaks a
/// 4.5MB OpenTUI shared library to `/tmp/.<hash>-NNNNNNNN.so` via bun:ffi
/// (anomalyco/opencode#6523), and the subprocess would just return the
/// same empty answer. Only fall back when the read itself fails (DB
/// missing, locked, schema drift, IO).
pub(crate) fn try_capture_opencode_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let entries_or_fallback =
        opencode_db_path().and_then(|p| read_opencode_sessions_from_sqlite_at(&p));

    match entries_or_fallback {
        Ok(entries) => {
            return select_opencode_session_from_values(
                &entries,
                project_path,
                exclusion,
                launch_time_ms,
            );
        }
        Err(e) => log_opencode_sqlite_fallback_once(&e),
    }

    let mut cmd = std::process::Command::new("opencode");
    cmd.args(["session", "list", "--format", "json"])
        .current_dir(project_path);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(OPENCODE_COMMAND_TIMEOUT_SECS),
        "opencode session list",
    )?;
    select_opencode_session(&stdout_bytes, project_path, exclusion, launch_time_ms)
}

/// Capture an OpenCode session ID from inside a Docker container.
///
/// Mirrors `try_capture_opencode_session_id` but runs `opencode session list`
/// via `docker exec -w <cwd>`. Matching is done against `container_cwd` (the
/// path opencode-in-container records as its working directory), not the host
/// project path.
pub(crate) fn try_capture_opencode_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "exec",
        "-w",
        container_cwd,
        container_name,
        "opencode",
        "session",
        "list",
        "--format",
        "json",
    ]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(OPENCODE_COMMAND_TIMEOUT_SECS),
        "opencode session list (container)",
    )?;
    select_opencode_session(&stdout_bytes, container_cwd, exclusion, launch_time_ms)
}

/// Polling closure for OpenCode session tracking.
pub(crate) fn opencode_poll_fn(
    project_path: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_opencode_session_id(&project_path, &exclusion, Some(launch_time_ms))
            .map_err(|e| tracing::debug!(target: "session.capture", "OpenCode poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

/// Polling closure for sandboxed (Docker) OpenCode session tracking.
pub(crate) fn opencode_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_opencode_session_id_in_container(
            &container_name,
            &container_cwd,
            &exclusion,
            Some(launch_time_ms),
        )
        .map_err(|e| tracing::debug!(target: "session.capture", "OpenCode container poll capture failed: {}", e))
        .ok()
        .and_then(validated_session_id)
    }
}

// ─── Codex CLI session capture ────────────────────────────────────────────────

const CODEX_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Shell snippet executed via `docker exec` to enumerate Codex `.jsonl` session
/// files inside the container. Each file is emitted as a
/// `===CODEX:<unix-mtime>:<basename>===` header followed by the first line of the
/// file and a `===END===` trailer.
const CODEX_CONTAINER_LIST_SCRIPT: &str = r#"SESS_DIR="${CODEX_HOME:-$HOME/.codex}/sessions"
[ -d "$SESS_DIR" ] || exit 0
find "$SESS_DIR" -name '*.jsonl' -type f | while read -r f; do
  ts=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)
  bn=$(basename "$f")
  printf '===CODEX:%s:%s===\n' "$ts" "$bn"
  head -n 1 "$f"
  printf '\n===END===\n'
done
"#;

/// Capture session ID from Codex filesystem.
///
/// Walks the Codex sessions directory (including date-partitioned `YYYY/MM/DD/` subdirectories)
/// for `.jsonl` rollout files and extracts the UUID from the most recent one.
/// Codex filenames follow the pattern `rollout-<timestamp>-<uuid>.jsonl`.
/// Respects `CODEX_HOME` env var, falling back to `~/.codex`.
pub(crate) fn capture_codex_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let codex_home = resolve_agent_home(Some("CODEX_HOME"), ".codex")?;
    let sessions_dir = codex_home.join("sessions");

    if !sessions_dir.exists() {
        anyhow::bail!(
            "Codex sessions directory not found: {}",
            sessions_dir.display()
        );
    }

    let mut session_entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    collect_codex_sessions(&sessions_dir, &mut session_entries)?;

    if session_entries.is_empty() {
        anyhow::bail!("No Codex sessions found in {}", sessions_dir.display());
    }

    session_entries.sort_by_key(|c| std::cmp::Reverse(c.1));

    let canonical_project = canonicalize_or_raw(project_path);

    let chosen = session_entries.iter().find_map(|(path, _)| {
        let uuid = extract_codex_uuid_from_filename(path)?;
        if exclusion.contains(&uuid) {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        let reader = std::io::BufReader::new(file);
        let first_line = std::io::BufRead::lines(reader).next()?.ok()?;
        let cwd = parse_codex_cwd_from_json(&first_line)?;
        let cwd_matches = std::fs::canonicalize(&cwd)
            .map(|c| c == canonical_project)
            .unwrap_or(false);
        if cwd_matches {
            Some(uuid)
        } else {
            None
        }
    });

    chosen.ok_or_else(|| anyhow::anyhow!("No Codex session found matching project path"))
}

/// Parse the CWD from a Codex `.jsonl` first line (already in memory).
///
/// Shared by the host scanner and the container scanner. Extracts `payload.cwd`
/// from the JSON object on the first line of a session file.
fn parse_codex_cwd_from_json(line: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    parsed
        .get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract UUID from a Codex rollout filename.
///
/// Codex filenames follow the pattern `rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl`.
/// The UUID is the last 36 characters of the stem (before `.jsonl`).
fn extract_codex_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() >= 36 {
        let candidate = &stem[stem.len() - 36..];
        if Uuid::parse_str(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Recursively collect Codex session `.jsonl` files, descending into date-partitioned dirs.
///
/// Directories whose names are all ASCII digits (e.g. `2025`, `03`, `06`) are treated as
/// date components and recursed into. Files ending in `.jsonl` are collected as session entries.
pub(crate) fn collect_codex_sessions(
    dir: &Path,
    entries: &mut Vec<(PathBuf, std::time::SystemTime)>,
) -> Result<()> {
    for entry in resilient_read_dir(dir)? {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.chars().all(|c| c.is_ascii_digit()) {
                collect_codex_sessions(&path, entries)?;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((path, modified));
        }
    }
    Ok(())
}

/// Capture a Codex session ID from inside a Docker container.
///
/// Mirrors `capture_codex_session_id` but reads `.jsonl` headers via
/// `docker exec sh` since codex-in-container writes to the container's
/// `~/.codex/sessions/`. Matches against `container_cwd` (the path
/// codex-in-container records), not the host project path.
pub(crate) fn try_capture_codex_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "exec",
        container_name,
        "sh",
        "-c",
        CODEX_CONTAINER_LIST_SCRIPT,
    ]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(CODEX_COMMAND_TIMEOUT_SECS),
        "docker exec sh (codex session scan)",
    )?;
    select_codex_session_in_container(&stdout_bytes, container_cwd, exclusion)
}

/// Parse the delimited stream emitted by `CODEX_CONTAINER_LIST_SCRIPT` and pick
/// the most recent session whose recorded CWD matches `container_cwd`.
fn select_codex_session_in_container(
    stdout_bytes: &[u8],
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = String::from_utf8_lossy(stdout_bytes);
    let mut candidates: Vec<(String, String, u64)> = Vec::new();

    for chunk in text.split("===CODEX:").skip(1) {
        let (header, rest) = match chunk.split_once("===\n") {
            Some(p) => p,
            None => continue,
        };
        let (ts_str, basename) = match header.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let ts: u64 = ts_str.trim().parse().unwrap_or(0);
        let uuid = match extract_codex_uuid_from_filename(Path::new(basename.trim())) {
            Some(u) if !exclusion.contains(&u) => u,
            _ => continue,
        };
        let json_part = match rest.split_once("\n===END===") {
            Some((j, _)) => j,
            None => rest,
        };
        let cwd = match parse_codex_cwd_from_json(json_part.trim()) {
            Some(c) => c,
            None => continue,
        };
        candidates.push((uuid, cwd, ts));
    }

    if candidates.is_empty() {
        anyhow::bail!("No Codex sessions found in container");
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));

    let project_match = candidates.iter().find(|(_, cwd, _)| cwd == container_cwd);

    project_match
        .map(|(id, _, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No Codex session matching container CWD"))
}

/// Polling closure for Codex CLI session tracking.
pub(crate) fn codex_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_codex_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Codex poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

/// Polling closure for sandboxed (Docker) Codex session tracking.
pub(crate) fn codex_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_codex_session_id_in_container(&container_name, &container_cwd, &exclusion)
            .map_err(|e| tracing::debug!(target: "session.capture", "Codex container poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

// ─── Gemini CLI session capture ───────────────────────────────────────────────

/// Polling closure for Gemini CLI session tracking.
pub(crate) fn gemini_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_gemini_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Gemini poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

const GEMINI_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Shell snippet executed via `docker exec` to enumerate Gemini session files
/// inside the container. Each file is emitted as a `===GEMINI:<unix-mtime>===`
/// header followed by the metadata-bearing first line and a `===END===` trailer.
///
/// Accepts both legacy `.json` (single-object) and current `.jsonl` (line-delimited)
/// formats. For both, the `sessionId` and `projectHash` we need live in the first
/// line, so `head -n 1` keeps the response small even for long conversations.
const GEMINI_CONTAINER_LIST_SCRIPT: &str = r#"GEMINI_HOME="${GEMINI_CLI_HOME:-$HOME/.gemini}"
TMP_DIR="$GEMINI_HOME/tmp"
[ -d "$TMP_DIR" ] || exit 0
find "$TMP_DIR" -type f \( -name 'session-*.json' -o -name 'session-*.jsonl' \) -path '*/chats/*' | while read -r f; do
  ts=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)
  printf '===GEMINI:%s===\n' "$ts"
  head -n 1 "$f"
  printf '\n===END===\n'
done
"#;

/// Capture a Gemini session ID from inside a Docker container.
///
/// Mirrors `capture_gemini_session_id` but reads session files via
/// `docker exec sh`. Matches against `expected_hash` (SHA-256 of the
/// container-side project path) rather than the host path.
pub(crate) fn try_capture_gemini_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(container_cwd.as_bytes());
    let expected_hash = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "exec",
        container_name,
        "sh",
        "-c",
        GEMINI_CONTAINER_LIST_SCRIPT,
    ]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(GEMINI_COMMAND_TIMEOUT_SECS),
        "docker exec sh (gemini session scan)",
    )?;
    select_gemini_session_in_container(&stdout_bytes, &expected_hash, exclusion)
}

/// Parse the delimited stream emitted by `GEMINI_CONTAINER_LIST_SCRIPT` and pick
/// the most recent session whose `projectHash` matches `expected_hash`.
fn select_gemini_session_in_container(
    stdout_bytes: &[u8],
    expected_hash: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = String::from_utf8_lossy(stdout_bytes);
    let mut candidates: Vec<(String, u64)> = Vec::new();

    for chunk in text.split("===GEMINI:").skip(1) {
        let (ts_str, rest) = match chunk.split_once("===\n") {
            Some(p) => p,
            None => continue,
        };
        let ts: u64 = ts_str.trim().parse().unwrap_or(0);
        let json_part = match rest.split_once("\n===END===") {
            Some((j, _)) => j,
            None => rest,
        };
        let (session_id, project_hash) = match parse_gemini_session_json(json_part.trim()) {
            Some((Some(sid), hash)) if !sid.is_empty() && !exclusion.contains(&sid) => (sid, hash),
            _ => continue,
        };
        if project_hash.as_deref() != Some(expected_hash) {
            continue;
        }
        candidates.push((session_id, ts));
    }

    if candidates.is_empty() {
        anyhow::bail!("No Gemini sessions found in container");
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
    Ok(candidates[0].0.clone())
}

/// Polling closure for sandboxed (Docker) Gemini session tracking.
pub(crate) fn gemini_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_gemini_session_id_in_container(&container_name, &container_cwd, &exclusion)
            .map_err(|e| tracing::debug!(target: "session.capture", "Gemini container poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

/// Capture Gemini session ID from `~/.gemini/tmp/<dir>/chats/session-*.json`.
///
/// `<dir>` is a SHA-256 hash of the project path. We compute it locally and look
/// for a matching directory, then scan all subdirs as a fallback verifying via the
/// `projectHash` JSON field.
pub(crate) fn capture_gemini_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    use sha2::{Digest, Sha256};

    let gemini_home = resolve_agent_home(Some("GEMINI_CLI_HOME"), ".gemini")?;
    let tmp_dir = gemini_home.join("tmp");

    if !tmp_dir.exists() {
        anyhow::bail!("Gemini tmp directory not found: {}", tmp_dir.display());
    }

    let canonical_project = canonicalize_or_raw(project_path);
    let digest = Sha256::digest(canonical_project.to_string_lossy().as_bytes());
    let expected_hash = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let project_dirs: Vec<std::path::PathBuf> = {
        let exact = tmp_dir.join(&expected_hash);
        if exact.is_dir() {
            vec![exact]
        } else {
            resilient_read_dir(&tmp_dir)?
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        }
    };

    let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime, Option<String>)> =
        Vec::new();

    for project_dir in &project_dirs {
        let chats_dir = project_dir.join("chats");
        if !chats_dir.is_dir() {
            continue;
        }

        let is_exact_match = project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == expected_hash);

        for chat_entry in resilient_read_dir(&chats_dir)? {
            let path = chat_entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("json") | Some("jsonl"))
                || !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("session-"))
            {
                continue;
            }

            let fields = extract_gemini_fields(&path);

            if !is_exact_match {
                let file_hash = fields
                    .as_ref()
                    .and_then(|(_, h)| h.as_deref())
                    .unwrap_or_default();
                if file_hash != expected_hash {
                    continue;
                }
            }

            let session_id = fields.and_then(|(sid, _)| sid);

            let modified = chat_entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((path, modified, session_id));
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("No Gemini session files found in {}", tmp_dir.display());
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

    candidates.retain(|(_, _, sid)| {
        sid.as_deref()
            .map(|id| !id.is_empty() && !exclusion.contains(id))
            .unwrap_or(false)
    });

    candidates
        .first()
        .and_then(|(_, _, sid)| sid.clone())
        .ok_or_else(|| anyhow::anyhow!("No Gemini session found matching project path"))
}

/// Extract session ID from a Gemini session JSON file, falling back to filename stem.
#[cfg(test)]
pub(crate) fn extract_gemini_session_id_from_file(path: &std::path::Path) -> Option<String> {
    extract_gemini_fields(path).and_then(|(sid, _)| sid)
}

/// Extract the project hash from a Gemini session file for CWD matching.
#[cfg(test)]
pub(crate) fn extract_gemini_project_hash_from_file(path: &std::path::Path) -> Option<String> {
    extract_gemini_fields(path).and_then(|(_, hash)| hash)
}

/// Parse the metadata of a Gemini session file (already in memory).
///
/// Handles both legacy single-object `.json` files (whole content is one object)
/// and current line-delimited `.jsonl` files (the metadata header is the first
/// line, with subsequent lines holding individual conversation records). Tries
/// to parse the whole content first, falling back to just the first line.
///
/// Shared by the host scanner and the container scanner, which receives the
/// metadata line via `docker exec` rather than a direct filesystem read.
/// Returns `(sessionId, projectHash)`.
fn parse_gemini_session_json(content: &str) -> Option<(Option<String>, Option<String>)> {
    let extract = |v: &serde_json::Value| {
        let session_id = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .map(String::from);
        let project_hash = v
            .get("projectHash")
            .and_then(|x| x.as_str())
            .map(String::from);
        (session_id, project_hash)
    };
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
        return Some(extract(&parsed));
    }
    let first_line = content.lines().next()?;
    let parsed: serde_json::Value = serde_json::from_str(first_line).ok()?;
    Some(extract(&parsed))
}

/// Read a Gemini session file once and return both sessionId and projectHash.
/// Falls back to filename stem for sessionId if the JSON field is absent.
fn extract_gemini_fields(path: &std::path::Path) -> Option<(Option<String>, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let (session_id, project_hash) = parse_gemini_session_json(&content)?;
    let session_id =
        session_id.or_else(|| path.file_stem().and_then(|s| s.to_str()).map(String::from));
    Some((session_id, project_hash))
}

// ─── Copilot CLI session capture ──────────────────────────────────────────────

/// Resolve the path to Copilot's local SQLite session store.
///
/// Copilot records every session in the `sessions` table of `session-store.db`
/// under its config dir (`$COPILOT_CONFIG_DIR`, default `~/.copilot`). Each row
/// carries the session UUID (`id`), the working directory (`cwd`), and an RFC
/// 3339 `updated_at` timestamp.
fn copilot_db_path() -> Result<PathBuf> {
    Ok(resolve_agent_home(Some("COPILOT_CONFIG_DIR"), ".copilot")?.join("session-store.db"))
}

/// Load Copilot's session rows from its SQLite store at `db_path`, newest
/// first. Each row is `(id, cwd)`; rows without a `cwd` are skipped since they
/// cannot be matched to a project. RFC 3339 `updated_at` strings sort
/// chronologically as text, so `ORDER BY updated_at DESC` yields most-recent
/// first.
fn read_copilot_sessions_from_sqlite_at(db_path: &Path) -> Result<Vec<(String, String)>> {
    use rusqlite::{Connection, OpenFlags};

    if !db_path.exists() {
        anyhow::bail!("Copilot session store not found at {}", db_path.display());
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "Failed to open Copilot session store at {}",
            db_path.display()
        )
    })?;
    conn.busy_timeout(Duration::from_millis(100))
        .context("Failed to set Copilot session store busy timeout")?;

    let mut stmt = conn
        .prepare("SELECT id, cwd FROM sessions WHERE cwd IS NOT NULL ORDER BY updated_at DESC")
        .context("Copilot sessions table schema mismatch")?;

    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let cwd: String = row.get(1)?;
            Ok((id, cwd))
        })
        .context("Failed to query Copilot sessions table")?;

    let mut entries: Vec<(String, String)> = Vec::new();
    for row in rows {
        entries.push(row.context("Failed to read Copilot session row")?);
    }
    Ok(entries)
}

/// Pick the newest unexcluded Copilot session whose `cwd` matches `match_path`.
///
/// `entries` are `(id, cwd)` pairs in newest-first order. Paths are compared
/// after canonicalization so a symlinked or `/tmp` -> `/private/tmp` cwd still
/// matches; a now-deleted cwd falls back to a raw string compare.
fn select_copilot_session(
    entries: &[(String, String)],
    match_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let canonical_match = canonicalize_or_raw(match_path);
    entries
        .iter()
        .find(|(id, cwd)| !exclusion.contains(id) && canonicalize_or_raw(cwd) == canonical_match)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("No Copilot session found matching project path"))
}

/// Capture a Copilot session ID for `project_path`.
///
/// Reads the `sessions` table of `~/.copilot/session-store.db` (or
/// `$COPILOT_CONFIG_DIR/session-store.db`) newest-first and returns the UUID of
/// the most recently updated session whose recorded `cwd` matches
/// `project_path`, skipping any IDs in `exclusion`. Copilot resumes that UUID
/// with `copilot --session-id <id>`.
pub(crate) fn capture_copilot_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let db_path = copilot_db_path()?;
    let entries = read_copilot_sessions_from_sqlite_at(&db_path)?;
    select_copilot_session(&entries, project_path, exclusion)
}

/// Polling closure for Copilot CLI session tracking.
pub(crate) fn copilot_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_copilot_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Copilot poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

// ─── Hermes session capture ───────────────────────────────────────────────────

const HERMES_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Python one-liner executed via `docker exec` to query active Hermes sessions.
/// Respects `$HERMES_HOME` env var, falling back to `~/.hermes/state.db`.
/// Prints one session ID per line to stdout.
const HERMES_CONTAINER_CAPTURE_SCRIPT: &str = "import sqlite3, os; \
db=os.path.join(os.environ.get('HERMES_HOME', os.path.expanduser('~/.hermes')), 'state.db'); \
conn=sqlite3.connect(db, timeout=1.0); \
[print(r[0]) for r in conn.execute(\"SELECT id FROM sessions WHERE source='cli' AND ended_at IS NULL ORDER BY started_at DESC LIMIT 10\")]";

/// Parse newline-separated session IDs from sqlite3/python3 output and return
/// the first ID not present in the exclusion set.
fn select_hermes_session(output: &[u8], exclusion: &HashSet<String>) -> Result<String> {
    let text = String::from_utf8_lossy(output);
    let id = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .find(|l| !exclusion.contains(*l));

    match id {
        Some(session_id) => Ok(session_id.to_string()),
        None => anyhow::bail!("No active Hermes session found"),
    }
}

/// Capture session ID from Hermes's SQLite state database.
///
/// Queries `~/.hermes/state.db` (or `$HERMES_HOME/state.db`) for active CLI
/// sessions. Unlike other agents, Hermes does not record the working directory
/// in its session table, so capture returns the most recent active CLI session
/// regardless of project. Cross-instance isolation relies on the exclusion set.
pub(crate) fn capture_hermes_session_id(
    _project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let hermes_home = resolve_agent_home(Some("HERMES_HOME"), ".hermes")?;
    let db_path = hermes_home.join("state.db");

    let ids = read_hermes_sessions_from_sqlite(&db_path)?;

    ids.into_iter()
        .find(|id| !exclusion.contains(id))
        .ok_or_else(|| anyhow::anyhow!("No active Hermes session found"))
}

/// Read active CLI session IDs from Hermes's SQLite state database.
///
/// Returns session IDs ordered by most recent first. An `Err` means the DB
/// is unreadable (missing, locked, schema mismatch); the poller will retry
/// on the next tick.
fn read_hermes_sessions_from_sqlite(db_path: &Path) -> Result<Vec<String>> {
    use rusqlite::{Connection, OpenFlags};

    if !db_path.exists() {
        anyhow::bail!("Hermes state.db not found at {}", db_path.display());
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Failed to open Hermes state.db at {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_millis(100))
        .context("Failed to set Hermes DB busy timeout")?;

    let mut stmt = conn
        .prepare(
            "SELECT id FROM sessions \
             WHERE source='cli' AND ended_at IS NULL \
             ORDER BY started_at DESC LIMIT 10",
        )
        .context("Hermes sessions table schema mismatch")?;

    let rows = stmt
        .query_map([], |row| row.get(0))
        .context("Failed to query Hermes sessions table")?;

    let mut ids: Vec<String> = Vec::new();
    for row in rows {
        ids.push(row.context("Failed to read Hermes session row")?);
    }

    Ok(ids)
}

/// Capture a Hermes session ID from inside a Docker container.
///
/// Uses `python3` (guaranteed by Hermes's Python runtime) rather than the
/// `sqlite3` CLI binary, which may not be installed in minimal containers.
pub(crate) fn try_capture_hermes_session_id_in_container(
    container_name: &str,
    _container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "exec",
        container_name,
        "python3",
        "-c",
        HERMES_CONTAINER_CAPTURE_SCRIPT,
    ]);

    let stdout_bytes = run_with_timeout(
        cmd,
        Duration::from_secs(HERMES_COMMAND_TIMEOUT_SECS),
        "docker exec python3 (hermes session scan)",
    )?;

    select_hermes_session(&stdout_bytes, exclusion)
}

/// Polling closure for Hermes session tracking.
pub(crate) fn hermes_poll_fn(
    project_path: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_hermes_session_id(&project_path, &exclusion)
            .map_err(
                |e| tracing::debug!(target: "session.capture", "Hermes poll capture failed: {}", e),
            )
            .ok()
            .and_then(validated_session_id)
    }
}

/// Polling closure for sandboxed (Docker) Hermes session tracking.
pub(crate) fn hermes_poll_fn_sandboxed(
    container_name: String,
    container_cwd: String,
    instance_id: String,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        try_capture_hermes_session_id_in_container(&container_name, &container_cwd, &exclusion)
            .map_err(|e| tracing::debug!(target: "session.capture", "Hermes container poll capture failed: {}", e))
            .ok()
            .and_then(validated_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_generate_claude_session_id() {
        let id = generate_claude_session_id();

        // Should be a valid UUID format
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_generate_claude_session_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_claude_session_id()).collect();
        let unique_ids: std::collections::HashSet<_> = ids.iter().collect();

        assert_eq!(ids.len(), unique_ids.len());
    }

    #[test]
    fn test_is_valid_session_id() {
        assert!(is_valid_session_id("abc-123"));
        assert!(is_valid_session_id("session_id.v2"));
        assert!(is_valid_session_id("a"));
        assert!(is_valid_session_id("ABC-def_123.456"));

        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("bad id!@#"));
        assert!(!is_valid_session_id("has space"));
        assert!(!is_valid_session_id("semi;colon"));
        assert!(!is_valid_session_id("back`tick"));
        assert!(!is_valid_session_id("path/slash"));
        assert!(!is_valid_session_id(&"x".repeat(257)));
    }

    #[test]
    fn test_encode_claude_project_path_basic() {
        assert_eq!(
            encode_claude_project_path("/Users/foo/bar"),
            "-Users-foo-bar"
        );
    }

    #[test]
    fn test_encode_claude_project_path_preserves_alphanumeric_and_dash() {
        assert_eq!(
            encode_claude_project_path("my-project-123"),
            "my-project-123"
        );
    }

    #[test]
    fn test_encode_claude_project_path_replaces_special_chars() {
        assert_eq!(
            encode_claude_project_path("/home/user/my project (copy)"),
            "-home-user-my-project--copy-"
        );
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_finds_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_old = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_new = "11111111-2222-3333-4444-555555555555";
        let old_file = project_dir.join(format!("{uuid_old}.jsonl"));
        let new_file = project_dir.join(format!("{uuid_new}.jsonl"));

        std::fs::write(&old_file, "old data\n").unwrap();
        // Set old file's mtime to 10 minutes ago
        let ten_min_ago = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&old_file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(ten_min_ago))
            .unwrap();
        std::fs::write(&new_file, "new data\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new());
        assert_eq!(result.unwrap(), uuid_new);

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_prefers_known_when_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_a = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_b = "11111111-2222-3333-4444-555555555555";
        std::fs::write(project_dir.join(format!("{uuid_a}.jsonl")), "a\n").unwrap();
        let a_time = std::time::SystemTime::now() - Duration::from_secs(30);
        std::fs::File::options()
            .write(true)
            .open(project_dir.join(format!("{uuid_a}.jsonl")))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(a_time))
            .unwrap();
        std::fs::write(project_dir.join(format!("{uuid_b}.jsonl")), "b\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", None, &HashSet::new()).unwrap(),
            uuid_b
        );

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_a), &HashSet::new()).unwrap(),
            uuid_b
        );

        let exclusion: HashSet<String> = std::iter::once(uuid_b.to_string()).collect();
        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_a), &exclusion).unwrap(),
            uuid_a
        );

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_b), &HashSet::new()).unwrap(),
            uuid_b
        );

        let absent = "99999999-9999-9999-9999-999999999999";
        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(absent), &HashSet::new()).unwrap(),
            uuid_b
        );

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_known_but_stale_falls_back() {
        // If our own session's file went stale (>5min), adopt the fresh
        // most-recent rather than clinging to a dead anchor.
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_known = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_fresh = "11111111-2222-3333-4444-555555555555";
        std::fs::write(project_dir.join(format!("{uuid_known}.jsonl")), "k\n").unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(project_dir.join(format!("{uuid_known}.jsonl")))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        std::fs::write(project_dir.join(format!("{uuid_fresh}.jsonl")), "f\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_known), &HashSet::new()).unwrap(),
            uuid_fresh
        );

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_claude_poll_fn_promotes_last_known_across_polls() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_startup = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_post_fork = "11111111-2222-3333-4444-555555555555";
        let uuid_sibling = "99999999-8888-7777-6666-555555555555";

        std::fs::write(project_dir.join(format!("{uuid_startup}.jsonl")), "s\n").unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(project_dir.join(format!("{uuid_startup}.jsonl")))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        std::fs::write(project_dir.join(format!("{uuid_post_fork}.jsonl")), "f\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let extra_excludes: HashSet<String> = std::iter::once(uuid_sibling.to_string()).collect();
        let poll = claude_poll_fn(
            "/tmp/myproject".to_string(),
            Some(uuid_startup.to_string()),
            "test-instance-promote-last-known".to_string(),
            extra_excludes,
        );

        assert_eq!(poll().as_deref(), Some(uuid_post_fork));

        std::fs::write(project_dir.join(format!("{uuid_sibling}.jsonl")), "x\n").unwrap();

        assert_eq!(poll().as_deref(), Some(uuid_post_fork));

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_skips_agent_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        std::fs::write(
            project_dir.join("agent-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "subagent data\n",
        )
        .unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new());
        assert!(result.is_err(), "Agent files should not be picked up");

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_rejects_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let file = project_dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&file, "old data\n").unwrap();

        // Set mtime to 10 minutes ago (beyond 5-minute threshold)
        let stale_time = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale_time))
            .unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new());
        assert!(result.is_err(), "Stale session file should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No active Claude session"),
            "Error should indicate no active session"
        );

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new());
        assert!(result.is_err(), "Empty dir should return error");

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    fn test_capture_claude_session_in_container_returns_error_for_missing_container() {
        let result = capture_claude_session_id_in_container(
            "aoe-test-nonexistent-container-xyz",
            "/workspace/test",
            &HashSet::new(),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_pi_project_path_basic() {
        assert_eq!(
            encode_pi_project_path("/home/user/project"),
            "--home-user-project--"
        );
    }

    #[test]
    fn test_encode_pi_project_path_with_dashes() {
        assert_eq!(
            encode_pi_project_path("/home/user/my-project"),
            "--home-user-my-project--"
        );
    }

    #[test]
    fn test_encode_pi_project_path_trailing_slash() {
        assert_eq!(
            encode_pi_project_path("/home/user/project/"),
            "--home-user-project---"
        );
    }

    #[test]
    fn test_encode_pi_project_path_double_slash() {
        assert_eq!(
            encode_pi_project_path("/a//double/slash"),
            "--a--double-slash--"
        );
    }

    #[test]
    fn test_encode_pi_project_path_spaces() {
        assert_eq!(
            encode_pi_project_path("/path/with spaces"),
            "--path-with spaces--"
        );
    }

    #[test]
    fn test_encode_pi_project_path_windows_backslash() {
        assert_eq!(
            encode_pi_project_path("C:\\Users\\bob\\proj"),
            "--C--Users-bob-proj--"
        );
    }

    #[test]
    fn test_encode_pi_project_path_colon() {
        assert_eq!(encode_pi_project_path("C:/Users/bob"), "--C--Users-bob--");
    }

    #[test]
    fn test_encode_pi_project_path_root() {
        assert_eq!(encode_pi_project_path("/"), "----");
    }

    #[test]
    fn test_extract_pi_session_id_from_header_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session","id":"019342ab-1234-7def-8901-abcdef012345","cwd":"/tmp"}"#,
        )
        .unwrap();
        assert_eq!(
            extract_pi_session_id_from_header(&path),
            Some("019342ab-1234-7def-8901-abcdef012345".to_string())
        );
    }

    #[test]
    fn test_extract_pi_session_id_from_header_missing_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(&path, r#"{"type":"session","cwd":"/tmp"}"#).unwrap();
        assert_eq!(extract_pi_session_id_from_header(&path), None);
    }

    #[test]
    fn test_extract_pi_session_id_from_header_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(&path, "not valid json at all").unwrap();
        assert_eq!(extract_pi_session_id_from_header(&path), None);
    }

    #[test]
    fn test_extract_pi_session_id_from_header_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(extract_pi_session_id_from_header(&path), None);
    }

    #[test]
    fn test_extract_pi_cwd_from_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session","id":"aaa","cwd":"/home/user/project"}"#,
        )
        .unwrap();
        assert_eq!(
            extract_pi_cwd_from_header(&path),
            Some("/home/user/project".to_string())
        );
    }

    #[test]
    fn test_extract_pi_uuid_from_filename() {
        let path =
            PathBuf::from("2024-12-03T14-00-00-000Z_019342ab-1234-7def-8901-abcdef012345.jsonl");
        assert_eq!(
            extract_pi_uuid_from_filename(&path),
            Some("019342ab-1234-7def-8901-abcdef012345".to_string())
        );
    }

    /// Real e2e: run the same shell script we ship to `docker exec` against a
    /// Pi session dir on disk, and feed the stdout into the parser to confirm
    /// it picks up the live UUID. Set `AOE_PI_E2E_DIR=/path/to/.pi/agent` and
    /// `AOE_PI_E2E_PROJECT=/abs/project/path` to enable; otherwise skipped.
    /// Validates the production `PI_CONTAINER_LIST_SCRIPT` against real Pi
    /// output without needing Docker.
    #[test]
    #[serial]
    fn test_select_pi_session_in_container_against_real_script_output() {
        let agent_dir = match std::env::var("AOE_PI_E2E_DIR") {
            Ok(v) => v,
            Err(_) => return,
        };
        let project_path = match std::env::var("AOE_PI_E2E_PROJECT") {
            Ok(v) => v,
            Err(_) => return,
        };

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(PI_CONTAINER_LIST_SCRIPT)
            .env("PI_CODING_AGENT_DIR", &agent_dir)
            .output()
            .expect("script invocation failed");
        assert!(
            output.status.success(),
            "script exited non-zero: {:?}",
            output.status
        );

        let id = select_pi_session_in_container(&output.stdout, &project_path, &HashSet::new())
            .expect("parser failed on real Pi output");
        assert!(
            Uuid::parse_str(&id).is_ok(),
            "captured id {id:?} is not a UUID"
        );
        eprintln!("captured pi session id via container script: {id}");
    }

    /// Real e2e: when run against a session dir produced by an actual `pi`
    /// binary, capture must return an ID that `pi --session <id>` accepts.
    /// Set `AOE_PI_E2E_DIR=/path/to/.pi/agent` and
    /// `AOE_PI_E2E_PROJECT=/abs/project/path` to enable; otherwise skipped.
    #[test]
    #[serial]
    fn test_capture_pi_session_id_against_real_pi_binary() {
        let agent_dir = match std::env::var("AOE_PI_E2E_DIR") {
            Ok(v) => v,
            Err(_) => return,
        };
        let project_path = match std::env::var("AOE_PI_E2E_PROJECT") {
            Ok(v) => v,
            Err(_) => return,
        };

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);

        let result = capture_pi_session_id(&project_path, &HashSet::new());

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }

        let id = result.expect("real Pi session capture failed");
        assert!(
            Uuid::parse_str(&id).is_ok(),
            "captured id {id:?} is not a UUID"
        );
        eprintln!("captured pi session id: {id}");
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let project_encoded = encode_pi_project_path("/home/user/project");
        let project_dir = sessions_dir.join(&project_encoded);
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid = "019342ab-1234-7def-8901-abcdef012345";
        std::fs::write(
            project_dir.join(format!("2024-12-03T14-00-00-000Z_{uuid}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let result = capture_pi_session_id("/home/user/project", &HashSet::new());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), uuid);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_most_recent_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let project_encoded = encode_pi_project_path("/home/user/project");
        let project_dir = sessions_dir.join(&project_encoded);
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_old = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_new = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        std::fs::write(
            project_dir.join(format!("2024-12-01T10-00-00-000Z_{uuid_old}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_old}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            project_dir.join(format!("2024-12-03T14-00-00-000Z_{uuid_new}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_new}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let result = capture_pi_session_id("/home/user/project", &HashSet::new());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), uuid_new);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let project_encoded = encode_pi_project_path("/home/user/project");
        let project_dir = sessions_dir.join(&project_encoded);
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_excluded = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_kept = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        std::fs::write(
            project_dir.join(format!("2024-12-01T10-00-00-000Z_{uuid_excluded}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_excluded}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();
        std::fs::write(
            project_dir.join(format!("2024-12-03T14-00-00-000Z_{uuid_kept}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_kept}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let mut exclusion = HashSet::new();
        exclusion.insert(uuid_excluded.to_string());

        let result = capture_pi_session_id("/home/user/project", &exclusion);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), uuid_kept);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_cwd_fallback_most_recent_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");

        let uuid_old = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_new = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let dir_a = sessions_dir.join("--wrong-name-a--");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::write(
            dir_a.join(format!("2024-12-01T10-00-00-000Z_{uuid_old}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_old}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let dir_b = sessions_dir.join("--wrong-name-b--");
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(
            dir_b.join(format!("2024-12-03T14-00-00-000Z_{uuid_new}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid_new}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let result = capture_pi_session_id("/home/user/project", &HashSet::new());
        assert!(
            result.is_ok(),
            "Fallback should find sessions via CWD header"
        );
        assert_eq!(result.unwrap(), uuid_new);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_cwd_fallback_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");

        let wrong_encoded = "--some-other-name--";
        let wrong_dir = sessions_dir.join(wrong_encoded);
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        std::fs::write(
            wrong_dir.join(format!("2024-12-03T14-00-00-000Z_{uuid}.jsonl")),
            format!(r#"{{"type":"session","id":"{uuid}","cwd":"/home/user/project"}}"#),
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let result = capture_pi_session_id("/home/user/project", &HashSet::new());
        assert!(result.is_ok(), "Fallback CWD scan should find the session");
        assert_eq!(result.unwrap(), uuid);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_pi_session_id_fallback_by_dir_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");

        let uuid_old = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_new = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let dir_old = sessions_dir.join("--old-dir--");
        std::fs::create_dir_all(&dir_old).unwrap();
        std::fs::write(
            dir_old.join(format!("2024-12-01T10-00-00-000Z_{uuid_old}.jsonl")),
            "not valid json\n",
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let dir_new = sessions_dir.join("--new-dir--");
        std::fs::create_dir_all(&dir_new).unwrap();
        std::fs::write(
            dir_new.join(format!("2024-12-03T14-00-00-000Z_{uuid_new}.jsonl")),
            "also not valid json\n",
        )
        .unwrap();

        let old_val = std::env::var("PI_CODING_AGENT_DIR").ok();
        std::env::set_var("PI_CODING_AGENT_DIR", tmp.path());

        let result = capture_pi_session_id("/nonexistent/path/for/test", &HashSet::new());
        assert!(
            result.is_ok(),
            "Dir-mtime fallback should find session: {:?}",
            result
        );
        assert_eq!(result.unwrap(), uuid_new);

        match old_val {
            Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
            None => std::env::remove_var("PI_CODING_AGENT_DIR"),
        }
    }

    /// Sets `VIBE_HOME` for the test's lifetime and restores it on Drop, so a
    /// panicking assertion can't leak the override into later serial tests.
    struct VibeHomeGuard {
        previous: Option<String>,
    }

    impl VibeHomeGuard {
        fn set(value: &Path) -> Self {
            let previous = std::env::var("VIBE_HOME").ok();
            std::env::set_var("VIBE_HOME", value);
            Self { previous }
        }
    }

    impl Drop for VibeHomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => std::env::set_var("VIBE_HOME", v),
                None => std::env::remove_var("VIBE_HOME"),
            }
        }
    }

    #[test]
    fn test_extract_vibe_meta_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        std::fs::write(
            &path,
            r#"{"session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "environment": {"working_directory": "/home/user/myrepo"}}"#,
        )
        .unwrap();
        assert_eq!(
            extract_vibe_meta(&path),
            Some((
                "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(),
                Some("/home/user/myrepo".to_string()),
            ))
        );
    }

    #[test]
    fn test_extract_vibe_meta_missing_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        std::fs::write(&path, r#"{"environment": {"working_directory": "/tmp"}}"#).unwrap();
        assert_eq!(extract_vibe_meta(&path), None);
    }

    #[test]
    fn test_extract_vibe_meta_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");
        assert_eq!(extract_vibe_meta(&path), None);
    }

    #[test]
    #[serial]
    fn test_vibe_capture_matches_by_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let sessions_dir = tmp.path().join("logs").join("session");

        // Session 1: matches our project
        let s1_dir = sessions_dir.join("session-abc");
        std::fs::create_dir_all(&s1_dir).unwrap();
        let s1_meta = serde_json::json!({
            "session_id": "vibe-sess-match",
            "environment": {"working_directory": project_dir.to_str().unwrap()}
        });
        std::fs::write(s1_dir.join("meta.json"), s1_meta.to_string()).unwrap();

        // Session 2: different project
        let s2_dir = sessions_dir.join("session-def");
        std::fs::create_dir_all(&s2_dir).unwrap();
        let s2_meta = serde_json::json!({
            "session_id": "vibe-sess-other",
            "environment": {"working_directory": "/somewhere/else"}
        });
        std::fs::write(s2_dir.join("meta.json"), s2_meta.to_string()).unwrap();

        let _guard = VibeHomeGuard::set(tmp.path());

        let exclusion = HashSet::new();
        let result = capture_vibe_session_id(project_dir.to_str().unwrap(), &exclusion);
        assert_eq!(result.unwrap(), "vibe-sess-match");
    }

    #[test]
    #[serial]
    fn test_vibe_stale_session_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let sessions_dir = tmp.path().join("logs").join("session");
        let s1_dir = sessions_dir.join("session-stale");
        std::fs::create_dir_all(&s1_dir).unwrap();

        // CWD points to a directory that doesn't exist (so canonicalize won't match)
        let s1_meta = serde_json::json!({
            "session_id": "vibe-sess-stale",
            "environment": {"working_directory": "/nonexistent/path/that/wont/match"}
        });
        std::fs::write(s1_dir.join("meta.json"), s1_meta.to_string()).unwrap();

        let _guard = VibeHomeGuard::set(tmp.path());

        let exclusion = HashSet::new();
        let result = capture_vibe_session_id(project_dir.to_str().unwrap(), &exclusion);
        assert!(
            result.is_err(),
            "Session with non-matching CWD should not be returned"
        );
    }

    #[test]
    #[serial]
    fn test_vibe_poll_fn_honors_extra_excludes() {
        // Regression for the resume-fallback cascade: after the cascade
        // clears a bad sid, the freshly-spawned poll closure must NOT
        // re-discover it via filesystem scan (the on-disk meta.json still
        // references the bad sid for several minutes). Without this,
        // `apply_session_id_updates` re-imports the cleared sid and the
        // cascade's work is undone within ~2s.
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let sessions_dir = tmp.path().join("logs").join("session");
        let s1_dir = sessions_dir.join("session-stale-on-disk");
        std::fs::create_dir_all(&s1_dir).unwrap();
        let s1_meta = serde_json::json!({
            "session_id": "stale-sid-cleared-by-cascade",
            "environment": {"working_directory": project_dir.to_str().unwrap()},
        });
        std::fs::write(s1_dir.join("meta.json"), s1_meta.to_string()).unwrap();

        let _guard = VibeHomeGuard::set(tmp.path());

        let mut extra = HashSet::new();
        extra.insert("stale-sid-cleared-by-cascade".to_string());
        let poll = vibe_poll_fn(
            project_dir.to_string_lossy().into_owned(),
            "test-instance".to_string(),
            extra,
        );
        assert_eq!(
            poll(),
            None,
            "poller must not re-import a sid present in extra_excludes",
        );

        let poll_no_excludes = vibe_poll_fn(
            project_dir.to_string_lossy().into_owned(),
            "test-instance".to_string(),
            HashSet::new(),
        );
        assert_eq!(
            poll_no_excludes(),
            Some("stale-sid-cleared-by-cascade".to_string()),
            "negative control: without the exclude, the poller surfaces the on-disk sid",
        );
    }

    #[test]
    fn test_select_vibe_session_in_container_picks_most_recent_match() {
        let stdout = b"\
===VIBE:1700000000===
{\"session_id\": \"older-match\", \"environment\": {\"working_directory\": \"/workspace\"}}
===END===
===VIBE:1700001000===
{\"session_id\": \"newer-match\", \"environment\": {\"working_directory\": \"/workspace\"}}
===END===
===VIBE:1700002000===
{\"session_id\": \"other-project\", \"environment\": {\"working_directory\": \"/elsewhere\"}}
===END===
";
        let result =
            select_vibe_session_in_container(stdout, "/workspace", &HashSet::new()).unwrap();
        assert_eq!(result, "newer-match");
    }

    #[test]
    fn test_select_vibe_session_in_container_respects_exclusion() {
        let stdout = b"\
===VIBE:1700001000===
{\"session_id\": \"already-claimed\", \"environment\": {\"working_directory\": \"/workspace\"}}
===END===
===VIBE:1700000500===
{\"session_id\": \"available\", \"environment\": {\"working_directory\": \"/workspace\"}}
===END===
";
        let mut exclusion = HashSet::new();
        exclusion.insert("already-claimed".to_string());
        let result = select_vibe_session_in_container(stdout, "/workspace", &exclusion).unwrap();
        assert_eq!(result, "available");
    }

    #[test]
    fn test_select_vibe_session_in_container_no_match_returns_error() {
        let stdout = b"\
===VIBE:1700000000===
{\"session_id\": \"foo\", \"environment\": {\"working_directory\": \"/somewhere/else\"}}
===END===
";
        let result = select_vibe_session_in_container(stdout, "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_vibe_session_in_container_empty_input() {
        let result = select_vibe_session_in_container(b"", "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_pi_session_in_container_picks_most_recent_match() {
        let stdout = b"\
===PI:1700000000===
{\"type\":\"session\",\"id\":\"older-match\",\"cwd\":\"/workspace\"}
===END===
===PI:1700001000===
{\"type\":\"session\",\"id\":\"newer-match\",\"cwd\":\"/workspace\"}
===END===
===PI:1700002000===
{\"type\":\"session\",\"id\":\"other-project\",\"cwd\":\"/elsewhere\"}
===END===
";
        let result = select_pi_session_in_container(stdout, "/workspace", &HashSet::new()).unwrap();
        assert_eq!(result, "newer-match");
    }

    #[test]
    fn test_select_pi_session_in_container_respects_exclusion() {
        let stdout = b"\
===PI:1700001000===
{\"type\":\"session\",\"id\":\"already-claimed\",\"cwd\":\"/workspace\"}
===END===
===PI:1700000500===
{\"type\":\"session\",\"id\":\"available\",\"cwd\":\"/workspace\"}
===END===
";
        let mut exclusion = HashSet::new();
        exclusion.insert("already-claimed".to_string());
        let result = select_pi_session_in_container(stdout, "/workspace", &exclusion).unwrap();
        assert_eq!(result, "available");
    }

    #[test]
    fn test_select_pi_session_in_container_no_match_returns_error() {
        let stdout = b"\
===PI:1700000000===
{\"type\":\"session\",\"id\":\"foo\",\"cwd\":\"/somewhere/else\"}
===END===
";
        let result = select_pi_session_in_container(stdout, "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_pi_session_in_container_empty_input() {
        let result = select_pi_session_in_container(b"", "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_pi_session_in_container_skips_non_session_lines() {
        let stdout = b"\
===PI:1700000000===
{\"type\":\"message\",\"id\":\"not-a-session\",\"cwd\":\"/workspace\"}
===END===
===PI:1700001000===
{\"type\":\"session\",\"id\":\"valid\",\"cwd\":\"/workspace\"}
===END===
";
        let result = select_pi_session_in_container(stdout, "/workspace", &HashSet::new()).unwrap();
        assert_eq!(result, "valid");
    }

    #[test]
    fn test_opencode_directory_matching() {
        let sessions_json = serde_json::json!([
            {"id": "wrong-session", "directory": "/home/user/other-project", "updated": 1735689600000_u64},
            {"id": "correct-session", "directory": "/tmp/my-project", "updated": 1735776000000_u64},
            {"id": "older-match", "directory": "/tmp/my-project", "updated": 1735689600000_u64},
        ]);
        let session_entries: Vec<serde_json::Value> =
            serde_json::from_value(sessions_json).unwrap();

        let matching = filter_agent_sessions(
            &session_entries,
            Some("/tmp/my-project"),
            &HashSet::new(),
            None,
        );

        let session = matching.first().copied();
        let id = session.and_then(|s| s["id"].as_str()).unwrap();

        assert_eq!(id, "correct-session");
        assert_eq!(matching.len(), 2);
    }

    #[test]
    fn test_opencode_exclusion_filters_claimed_sessions() {
        let sessions_json = serde_json::json!([
            {"id": "best-session", "directory": "/tmp/my-project", "updated": 1735776000000_u64},
            {"id": "second-best", "directory": "/tmp/my-project", "updated": 1735775000000_u64},
        ]);
        let session_entries: Vec<serde_json::Value> =
            serde_json::from_value(sessions_json).unwrap();

        let mut exclusion = HashSet::new();
        exclusion.insert("best-session".to_string());

        let matching =
            filter_agent_sessions(&session_entries, Some("/tmp/my-project"), &exclusion, None);

        let session = matching.first().copied();
        let id = session.and_then(|s| s["id"].as_str()).unwrap();
        assert_eq!(id, "second-best");
    }

    #[test]
    fn test_opencode_no_match_returns_error() {
        let sessions_json = serde_json::json!([
            {"id": "sess-1", "directory": "/tmp/my-project", "updated": 1735776000000_u64},
            {"id": "sess-2", "directory": "/tmp/my-project", "updated": 1735775000000_u64},
        ]);
        let session_entries: Vec<serde_json::Value> =
            serde_json::from_value(sessions_json).unwrap();

        let mut exclusion = HashSet::new();
        exclusion.insert("sess-1".to_string());
        exclusion.insert("sess-2".to_string());

        let matching =
            filter_agent_sessions(&session_entries, Some("/tmp/my-project"), &exclusion, None);

        assert!(
            matching.is_empty(),
            "All sessions are excluded, matching should be empty (not fallback to first)"
        );
    }

    #[test]
    fn test_opencode_timestamp_guard() {
        let sessions_json = serde_json::json!([
            {"id": "old-session", "directory": "/tmp/my-project", "updated": 1000000000000_u64},
            {"id": "new-session", "directory": "/tmp/my-project", "updated": 1735776000000_u64},
            {"id": "stale-session", "directory": "/tmp/my-project", "updated": 1500000000000_u64},
        ]);
        let session_entries: Vec<serde_json::Value> =
            serde_json::from_value(sessions_json).unwrap();

        let launch_time_ms: f64 = 1735000000000.0;
        let exclusion: HashSet<String> = HashSet::new();

        let matching = filter_agent_sessions(
            &session_entries,
            Some("/tmp/my-project"),
            &exclusion,
            Some(launch_time_ms),
        );

        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0]["id"].as_str().unwrap(), "new-session");
    }

    #[test]
    fn test_filter_agent_sessions_empty_input() {
        let empty: Vec<serde_json::Value> = Vec::new();
        let exclusion = HashSet::new();
        let result = filter_agent_sessions(&empty, Some("/tmp/project"), &exclusion, None);
        assert!(
            result.is_empty(),
            "Empty input should return empty result, not panic"
        );
    }

    #[test]
    fn test_build_exclusion_set_empty() {
        let result = build_exclusion_set("nonexistent-instance-id-12345");
        // The exclusion set should never contain our own instance ID
        // (it collects OTHER instances' captured session IDs).
        // On a machine with active AoE tmux sessions, the set may be
        // non-empty, so we verify our own ID isn't self-excluded.
        assert!(!result.contains("nonexistent-instance-id-12345"));
    }

    #[test]
    fn test_opencode_capture_respects_command_timeout() {
        let start = std::time::Instant::now();
        let result = try_capture_opencode_session_id(
            "/tmp/nonexistent-project-xyz-12345",
            &HashSet::new(),
            None,
        );
        let elapsed = start.elapsed();

        assert!(result.is_err(), "Expected Err for nonexistent project");
        assert!(
            elapsed < Duration::from_secs(OPENCODE_COMMAND_TIMEOUT_SECS + 2),
            "Capture took {:?}, exceeds timeout budget",
            elapsed
        );
    }

    // ─── Codex tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_extract_codex_uuid_from_filename() {
        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        let path = PathBuf::from(format!("rollout-2025-03-06T12-00-00-{}.jsonl", uuid));
        assert_eq!(
            extract_codex_uuid_from_filename(&path),
            Some(uuid.to_string())
        );
    }

    #[test]
    fn test_extract_codex_uuid_non_standard_filename_returns_none() {
        let path = PathBuf::from("my-thread-name.jsonl");
        assert_eq!(extract_codex_uuid_from_filename(&path), None);
    }

    #[test]
    fn test_parse_codex_cwd_from_json() {
        let line = r#"{"type":"session_meta","payload":{"cwd":"/home/user/myproject"}}"#;
        assert_eq!(
            parse_codex_cwd_from_json(line),
            Some("/home/user/myproject".to_string())
        );
    }

    #[test]
    fn test_parse_codex_cwd_from_json_missing_field() {
        let line = r#"{"type":"session_meta","payload":{}}"#;
        assert_eq!(parse_codex_cwd_from_json(line), None);
    }

    #[test]
    fn test_parse_codex_cwd_from_json_invalid_json() {
        assert_eq!(parse_codex_cwd_from_json("not json at all"), None);
    }

    #[test]
    fn test_collect_codex_sessions_walks_date_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");

        let date_path = sessions_dir.join("2025").join("03").join("06");
        std::fs::create_dir_all(&date_path).unwrap();

        let uuid_deep = "dddddddd-dddd-dddd-dddd-dddddddddddd";
        let uuid_flat = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";
        std::fs::write(
            date_path.join(format!("rollout-2025-03-06T12-00-00-{}.jsonl", uuid_deep)),
            "{}",
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{}.jsonl", uuid_flat)),
            "{}",
        )
        .unwrap();

        let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        collect_codex_sessions(&sessions_dir, &mut entries).unwrap();

        let uuids: Vec<String> = entries
            .iter()
            .filter_map(|(p, _)| extract_codex_uuid_from_filename(p))
            .collect();

        assert!(uuids.contains(&uuid_deep.to_string()));
        assert!(uuids.contains(&uuid_flat.to_string()));
        assert_eq!(uuids.len(), 2);
    }

    #[test]
    fn test_collect_codex_sessions_most_recent_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let uuid_old = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_new = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let old_file = sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{}.jsonl", uuid_old));
        let new_file = sessions_dir.join(format!("rollout-2025-01-02T00-00-00-{}.jsonl", uuid_new));
        std::fs::write(&old_file, "{}").unwrap();
        std::fs::write(&new_file, "{}").unwrap();

        let old_time = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&old_file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();

        let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        collect_codex_sessions(&sessions_dir, &mut entries).unwrap();
        entries.sort_by_key(|c| std::cmp::Reverse(c.1));

        let selected = entries
            .first()
            .and_then(|(p, _)| extract_codex_uuid_from_filename(p))
            .unwrap();
        assert_eq!(selected, uuid_new);
    }

    struct CodexHomeGuard(Option<String>);
    impl CodexHomeGuard {
        fn set(path: &str) -> Self {
            let prev = std::env::var("CODEX_HOME").ok();
            std::env::set_var("CODEX_HOME", path);
            Self(prev)
        }
    }
    impl Drop for CodexHomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }

    #[test]
    #[serial]
    fn test_codex_respects_codex_home_env() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let uuid = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let project_dir = tmp.path().join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_content = format!(
            r#"{{"type":"session_meta","payload":{{"cwd":"{}"}}}}"#,
            project_dir.display()
        );
        std::fs::write(
            sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{}.jsonl", uuid)),
            jsonl_content,
        )
        .unwrap();

        let _guard = CodexHomeGuard::set(tmp.path().to_str().unwrap());

        let result = capture_codex_session_id(project_dir.to_str().unwrap(), &HashSet::new());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), uuid);
    }

    #[test]
    #[serial]
    fn test_codex_capture_empty_sessions_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let _guard = CodexHomeGuard::set(tmp.path().to_str().unwrap());

        let result = capture_codex_session_id("/tmp/some-project", &HashSet::new());
        assert!(result.is_err(), "Empty sessions dir should return error");
    }

    #[test]
    fn test_select_codex_session_in_container_most_recent() {
        let uuid_old = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_new = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let uuid_other = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let stdout = format!(
            "\
===CODEX:1700000000:rollout-2025-01-01T00-00-00-{uuid_old}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/workspace\"}}}}
===END===
===CODEX:1700001000:rollout-2025-01-02T00-00-00-{uuid_new}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/workspace\"}}}}
===END===
===CODEX:1700002000:rollout-2025-01-03T00-00-00-{uuid_other}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/elsewhere\"}}}}
===END===
"
        );
        let result =
            select_codex_session_in_container(stdout.as_bytes(), "/workspace", &HashSet::new())
                .unwrap();
        assert_eq!(result, uuid_new);
    }

    #[test]
    fn test_select_codex_session_in_container_exclusion() {
        let uuid_claimed = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_available = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let stdout = format!(
            "\
===CODEX:1700001000:rollout-2025-01-02T00-00-00-{uuid_claimed}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/workspace\"}}}}
===END===
===CODEX:1700000500:rollout-2025-01-01T00-00-00-{uuid_available}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/workspace\"}}}}
===END===
"
        );
        let mut exclusion = HashSet::new();
        exclusion.insert(uuid_claimed.to_string());
        let result =
            select_codex_session_in_container(stdout.as_bytes(), "/workspace", &exclusion).unwrap();
        assert_eq!(result, uuid_available);
    }

    #[test]
    fn test_select_codex_session_in_container_no_match() {
        let uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let stdout = format!(
            "\
===CODEX:1700000000:rollout-2025-01-01T00-00-00-{uuid}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/somewhere/else\"}}}}
===END===
"
        );
        let result =
            select_codex_session_in_container(stdout.as_bytes(), "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_codex_session_in_container_empty_input() {
        let result = select_codex_session_in_container(b"", "/workspace", &HashSet::new());
        assert!(result.is_err());
    }

    // ─── Gemini tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_extract_gemini_session_id_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session-42.json");
        std::fs::write(
            &path,
            r#"{"sessionId": "abc-123", "projectHash": "deadbeef"}"#,
        )
        .unwrap();
        assert_eq!(
            extract_gemini_session_id_from_file(&path),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn test_extract_gemini_session_id_from_file_falls_back_to_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session-42.json");
        std::fs::write(&path, r#"{"projectHash": "deadbeef"}"#).unwrap();
        assert_eq!(
            extract_gemini_session_id_from_file(&path),
            Some("session-42".to_string())
        );
    }

    #[test]
    fn test_extract_gemini_session_id_from_file_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session-42.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(extract_gemini_session_id_from_file(&path), None);
    }

    #[test]
    fn test_extract_gemini_project_hash_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"sessionId": "s1", "projectHash": "abc123def456"}"#,
        )
        .unwrap();
        assert_eq!(
            extract_gemini_project_hash_from_file(&path),
            Some("abc123def456".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_gemini_capture_returns_most_recent_by_cwd() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let project_path = "/tmp/gemini-test-project";
        let digest = Sha256::digest(project_path.as_bytes());
        let hash = digest
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let chats_dir = tmp.path().join("tmp").join(&hash).join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let old_file = chats_dir.join("session-1.json");
        std::fs::write(
            &old_file,
            format!(r#"{{"sessionId": "old-id-111", "projectHash": "{hash}"}}"#),
        )
        .unwrap();
        let ten_min_ago = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&old_file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(ten_min_ago))
            .unwrap();

        let new_file = chats_dir.join("session-2.json");
        std::fs::write(
            &new_file,
            format!(r#"{{"sessionId": "new-id-222", "projectHash": "{hash}"}}"#),
        )
        .unwrap();

        let _guard = GeminiHomeGuard::set(tmp.path());

        let result = capture_gemini_session_id(project_path, &HashSet::new());
        assert_eq!(result.unwrap(), "new-id-222");
    }

    #[test]
    #[serial]
    fn test_gemini_exclusion_uses_json_id_not_stem() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let project_path = "/tmp/gemini-exclusion-test";
        let digest = Sha256::digest(project_path.as_bytes());
        let hash = digest
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let chats_dir = tmp.path().join("tmp").join(&hash).join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let file1 = chats_dir.join("session-1.json");
        std::fs::write(
            &file1,
            format!(r#"{{"sessionId": "json-id-AAA", "projectHash": "{hash}"}}"#),
        )
        .unwrap();

        let file2 = chats_dir.join("session-2.json");
        std::fs::write(
            &file2,
            format!(r#"{{"sessionId": "json-id-BBB", "projectHash": "{hash}"}}"#),
        )
        .unwrap();
        let older = std::time::SystemTime::now() - Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&file2)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(older))
            .unwrap();

        let _guard = GeminiHomeGuard::set(tmp.path());

        let mut exclusion = HashSet::new();
        exclusion.insert("json-id-AAA".to_string());

        let result = capture_gemini_session_id(project_path, &exclusion);
        assert_eq!(
            result.unwrap(),
            "json-id-BBB",
            "Exclusion must use JSON sessionId, not filename stem"
        );

        let mut wrong_exclusion = HashSet::new();
        wrong_exclusion.insert("session-1".to_string());

        let result2 = capture_gemini_session_id(project_path, &wrong_exclusion);
        assert_eq!(
            result2.unwrap(),
            "json-id-AAA",
            "Filename stem in exclusion should have no effect"
        );
    }

    struct GeminiHomeGuard {
        previous: Option<String>,
    }

    impl GeminiHomeGuard {
        fn set(value: &Path) -> Self {
            let previous = std::env::var("GEMINI_CLI_HOME").ok();
            std::env::set_var("GEMINI_CLI_HOME", value);
            Self { previous }
        }
    }

    impl Drop for GeminiHomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => std::env::set_var("GEMINI_CLI_HOME", v),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
    }

    #[test]
    fn test_select_gemini_session_in_container_most_recent() {
        let stdout = b"\
===GEMINI:1700000000===
{\"sessionId\": \"older-match\", \"projectHash\": \"abc123\"}
===END===
===GEMINI:1700001000===
{\"sessionId\": \"newer-match\", \"projectHash\": \"abc123\"}
===END===
===GEMINI:1700002000===
{\"sessionId\": \"other-project\", \"projectHash\": \"def456\"}
===END===
";
        let result = select_gemini_session_in_container(stdout, "abc123", &HashSet::new()).unwrap();
        assert_eq!(result, "newer-match");
    }

    #[test]
    fn test_select_gemini_session_in_container_exclusion() {
        let stdout = b"\
===GEMINI:1700001000===
{\"sessionId\": \"already-claimed\", \"projectHash\": \"abc123\"}
===END===
===GEMINI:1700000500===
{\"sessionId\": \"available\", \"projectHash\": \"abc123\"}
===END===
";
        let mut exclusion = HashSet::new();
        exclusion.insert("already-claimed".to_string());
        let result = select_gemini_session_in_container(stdout, "abc123", &exclusion).unwrap();
        assert_eq!(result, "available");
    }

    #[test]
    fn test_select_gemini_session_in_container_no_match() {
        let stdout = b"\
===GEMINI:1700000000===
{\"sessionId\": \"foo\", \"projectHash\": \"wrong-hash\"}
===END===
";
        let result = select_gemini_session_in_container(stdout, "abc123", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_gemini_session_in_container_empty_input() {
        let result = select_gemini_session_in_container(b"", "abc123", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_gemini_session_json_handles_jsonl_first_line() {
        // current gemini-cli (>= 0.40) writes line-delimited files where the
        // first line is the metadata header and subsequent lines are records
        let content = "\
{\"sessionId\":\"abc-123\",\"projectHash\":\"deadbeef\",\"startTime\":\"2026-04-29T19:06:25.028Z\",\"kind\":\"main\"}
{\"role\":\"user\",\"content\":\"hello\"}
{\"role\":\"assistant\",\"content\":\"hi\"}
";
        let (sid, hash) = parse_gemini_session_json(content).unwrap();
        assert_eq!(sid.as_deref(), Some("abc-123"));
        assert_eq!(hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    #[serial]
    fn test_gemini_capture_handles_jsonl_in_short_id_dir() {
        // simulates current gemini-cli layout: $HOME/.gemini/tmp/<short-id>/chats/
        // containing .jsonl files, where the project subdir name is *not* the
        // sha256 of the cwd but a registry short-id. The fallback that scans all
        // subdirs and matches by the file's `projectHash` field must still work.
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let project_path = "/tmp/gemini-jsonl-test";
        let digest = Sha256::digest(project_path.as_bytes());
        let project_hash = digest
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let chats_dir = tmp.path().join("tmp").join("short-id-abc").join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let session_file = chats_dir.join("session-2026-04-29T19-06-deadbeef.jsonl");
        let body = format!(
            "{{\"sessionId\":\"jsonl-session-id\",\"projectHash\":\"{project_hash}\",\"startTime\":\"2026-04-29T19:06:25.028Z\",\"kind\":\"main\"}}\n\
{{\"role\":\"user\",\"content\":\"hello\"}}\n"
        );
        std::fs::write(&session_file, body).unwrap();

        let _guard = GeminiHomeGuard::set(tmp.path());

        let result = capture_gemini_session_id(project_path, &HashSet::new());
        assert_eq!(result.unwrap(), "jsonl-session-id");
    }

    #[test]
    fn test_select_gemini_session_in_container_jsonl_first_line() {
        // container script now emits only the first line of each session file via
        // `head -n 1`, so it works for both .json and .jsonl. Verify the parser
        // handles a single metadata-line response.
        let stdout = b"\
===GEMINI:1700001000===
{\"sessionId\":\"jsonl-id\",\"projectHash\":\"abc123\",\"kind\":\"main\"}
===END===
";
        let result = select_gemini_session_in_container(stdout, "abc123", &HashSet::new()).unwrap();
        assert_eq!(result, "jsonl-id");
    }

    // ─── Hermes tests ────────────────────────────────────────────────────────────

    #[test]
    fn test_select_hermes_session_parsing() {
        let output = b"20260429_193246_aaa\n20260429_193246_bbb\n";
        let exclusion = HashSet::new();
        let result = select_hermes_session(output, &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    fn test_select_hermes_session_with_exclusion() {
        let output = b"20260429_193246_aaa\n20260429_193246_bbb\n";
        let mut exclusion = HashSet::new();
        exclusion.insert("20260429_193246_aaa".to_string());
        let result = select_hermes_session(output, &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_bbb");
    }

    #[test]
    fn test_select_hermes_session_empty_output() {
        let output = b"";
        let exclusion = HashSet::new();
        let result = select_hermes_session(output, &exclusion);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_hermes_session_whitespace_lines() {
        let output = b"  \n\n20260429_193246_ccc\n  \n";
        let exclusion = HashSet::new();
        let result = select_hermes_session(output, &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_ccc");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
             INSERT INTO sessions VALUES ('20260429_193246_adcddd','cli',1000.0,NULL);",
        )
        .unwrap();
        drop(conn);
        unsafe { std::env::set_var("HERMES_HOME", tmp.path()) };
        let exclusion = HashSet::new();
        let result = capture_hermes_session_id(".", &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_adcddd");
        unsafe { std::env::remove_var("HERMES_HOME") };
    }

    #[test]
    #[serial]
    fn test_capture_hermes_excludes_ended() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
             INSERT INTO sessions VALUES ('ended_session','cli',1000.0,123456.0);",
        )
        .unwrap();
        drop(conn);
        unsafe { std::env::set_var("HERMES_HOME", tmp.path()) };
        let result = capture_hermes_session_id(".", &HashSet::new());
        assert!(result.is_err());
        unsafe { std::env::remove_var("HERMES_HOME") };
    }

    #[test]
    #[serial]
    fn test_capture_hermes_excludes_non_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
             INSERT INTO sessions VALUES ('telegram_session','telegram',1000.0,NULL);",
        )
        .unwrap();
        drop(conn);
        unsafe { std::env::set_var("HERMES_HOME", tmp.path()) };
        let result = capture_hermes_session_id(".", &HashSet::new());
        assert!(result.is_err());
        unsafe { std::env::remove_var("HERMES_HOME") };
    }

    #[test]
    #[serial]
    fn test_capture_hermes_exclusion_set() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
             INSERT INTO sessions VALUES ('first_session','cli',2000.0,NULL);
             INSERT INTO sessions VALUES ('second_session','cli',1000.0,NULL);",
        )
        .unwrap();
        drop(conn);
        unsafe { std::env::set_var("HERMES_HOME", tmp.path()) };
        let mut exclusion = HashSet::new();
        exclusion.insert("first_session".to_string());
        let result = capture_hermes_session_id(".", &exclusion).unwrap();
        assert_eq!(result, "second_session");
        unsafe { std::env::remove_var("HERMES_HOME") };
    }

    #[test]
    #[serial]
    fn test_capture_hermes_no_db() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HERMES_HOME", tmp.path()) };
        let result = capture_hermes_session_id(".", &HashSet::new());
        assert!(result.is_err());
        unsafe { std::env::remove_var("HERMES_HOME") };
    }

    #[test]
    fn test_hermes_session_id_format_valid() {
        assert!(is_valid_session_id("20260429_193246_adcddd"));
    }

    fn create_copilot_test_db(rows: &[(&str, &str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-store.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                updated_at TEXT
            );",
        )
        .unwrap();
        for (id, cwd, updated_at) in rows {
            conn.execute(
                "INSERT INTO sessions (id, cwd, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, cwd, updated_at],
            )
            .unwrap();
        }
        drop(conn);
        dir
    }

    #[test]
    fn test_select_copilot_session_matches_cwd_newest_first() {
        let entries = vec![
            ("newer".to_string(), "/work/proj".to_string()),
            ("older".to_string(), "/work/proj".to_string()),
            ("other".to_string(), "/work/elsewhere".to_string()),
        ];
        // ORDER BY updated_at DESC already put the newest match first.
        let result = select_copilot_session(&entries, "/work/proj", &HashSet::new()).unwrap();
        assert_eq!(result, "newer");
    }

    #[test]
    fn test_select_copilot_session_skips_excluded() {
        let entries = vec![
            ("newer".to_string(), "/work/proj".to_string()),
            ("older".to_string(), "/work/proj".to_string()),
        ];
        let exclusion: HashSet<String> = ["newer".to_string()].into_iter().collect();
        let result = select_copilot_session(&entries, "/work/proj", &exclusion).unwrap();
        assert_eq!(result, "older");
    }

    #[test]
    fn test_select_copilot_session_no_cwd_match_errs() {
        let entries = vec![("a".to_string(), "/work/elsewhere".to_string())];
        let result = select_copilot_session(&entries, "/work/proj", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_copilot_capture_basic() {
        let tmp = create_copilot_test_db(&[
            (
                "11111111-1111-4111-8111-111111111111",
                "/work/proj",
                "2026-06-28T10:00:00.000Z",
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                "/work/proj",
                "2026-06-28T12:00:00.000Z",
            ),
        ]);
        unsafe { std::env::set_var("COPILOT_CONFIG_DIR", tmp.path()) };
        let result = capture_copilot_session_id("/work/proj", &HashSet::new()).unwrap();
        unsafe { std::env::remove_var("COPILOT_CONFIG_DIR") };
        // Newest updated_at wins.
        assert_eq!(result, "22222222-2222-4222-8222-222222222222");
    }

    #[test]
    #[serial]
    fn test_copilot_capture_filters_by_project_path() {
        let tmp = create_copilot_test_db(&[
            (
                "33333333-3333-4333-8333-333333333333",
                "/work/other",
                "2026-06-28T12:00:00.000Z",
            ),
            (
                "44444444-4444-4444-8444-444444444444",
                "/work/proj",
                "2026-06-28T10:00:00.000Z",
            ),
        ]);
        unsafe { std::env::set_var("COPILOT_CONFIG_DIR", tmp.path()) };
        let result = capture_copilot_session_id("/work/proj", &HashSet::new()).unwrap();
        unsafe { std::env::remove_var("COPILOT_CONFIG_DIR") };
        assert_eq!(result, "44444444-4444-4444-8444-444444444444");
    }

    #[test]
    #[serial]
    fn test_copilot_capture_no_db() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("COPILOT_CONFIG_DIR", tmp.path()) };
        let result = capture_copilot_session_id("/work/proj", &HashSet::new());
        unsafe { std::env::remove_var("COPILOT_CONFIG_DIR") };
        assert!(result.is_err());
    }

    fn create_opencode_test_db(rows: &[(&str, &str, i64)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL,
                time_updated INTEGER NOT NULL
            );",
        )
        .unwrap();
        for (id, directory, ts) in rows {
            conn.execute(
                "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, directory, ts],
            )
            .unwrap();
        }
        (dir, db_path)
    }

    #[test]
    fn test_opencode_sqlite_picks_most_recent_match() {
        let project = std::env::current_dir().unwrap();
        let project_str = project.to_string_lossy().to_string();
        let other = "/tmp/other-project";
        let (_dir, db_path) = create_opencode_test_db(&[
            ("ses_old", &project_str, 1_000_000),
            ("ses_other", other, 9_999_999),
            ("ses_new", &project_str, 2_000_000),
        ]);

        let entries = read_opencode_sessions_from_sqlite_at(&db_path).unwrap();
        let result =
            select_opencode_session_from_values(&entries, &project_str, &HashSet::new(), None)
                .unwrap();
        assert_eq!(result, "ses_new");
    }

    #[test]
    fn test_opencode_sqlite_excludes_known_ids() {
        let project = std::env::current_dir().unwrap();
        let project_str = project.to_string_lossy().to_string();
        let (_dir, db_path) = create_opencode_test_db(&[
            ("ses_skip", &project_str, 2_000_000),
            ("ses_keep", &project_str, 1_000_000),
        ]);

        let mut exclusion = HashSet::new();
        exclusion.insert("ses_skip".to_string());
        let entries = read_opencode_sessions_from_sqlite_at(&db_path).unwrap();
        let result =
            select_opencode_session_from_values(&entries, &project_str, &exclusion, None).unwrap();
        assert_eq!(result, "ses_keep");
    }

    #[test]
    fn test_opencode_sqlite_respects_launch_time_floor() {
        let project = std::env::current_dir().unwrap();
        let project_str = project.to_string_lossy().to_string();
        let (_dir, db_path) = create_opencode_test_db(&[
            ("ses_stale", &project_str, 1_000),
            ("ses_fresh", &project_str, 5_000),
        ]);

        let entries = read_opencode_sessions_from_sqlite_at(&db_path).unwrap();
        let result = select_opencode_session_from_values(
            &entries,
            &project_str,
            &HashSet::new(),
            Some(2_000.0),
        )
        .unwrap();
        assert_eq!(result, "ses_fresh");
    }

    #[test]
    fn test_opencode_sqlite_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("does-not-exist.db");
        let result = read_opencode_sessions_from_sqlite_at(&db_path);
        assert!(result.is_err(), "missing DB must Err so caller falls back");
    }

    #[test]
    fn test_opencode_sqlite_schema_mismatch_errors_for_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // Missing required columns; the prepare() should fail and we should
        // bubble up an error so the caller falls back to the subprocess path.
        conn.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY);")
            .unwrap();
        drop(conn);

        let result = read_opencode_sessions_from_sqlite_at(&db_path);
        assert!(
            result.is_err(),
            "schema mismatch must Err so caller falls back"
        );
    }

    #[test]
    fn test_opencode_sqlite_no_matching_directory_returns_no_match_not_infra_error() {
        // Critical for not re-introducing the leak: when the DB is readable
        // but no session matches, the inner reader must succeed (so the
        // public function does NOT fall back to the leaky subprocess), and
        // the selector returns the genuine "no match" error.
        let (_dir, db_path) = create_opencode_test_db(&[("ses_x", "/elsewhere", 1)]);
        let entries = read_opencode_sessions_from_sqlite_at(&db_path)
            .expect("readable DB must Ok even when no rows match the project path");
        assert_eq!(entries.len(), 1);
        let result =
            select_opencode_session_from_values(&entries, "/different", &HashSet::new(), None);
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_opencode_db_path_respects_opencode_db_env_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let custom_path = tmp.path().join("custom.db");
        std::fs::write(&custom_path, "").unwrap();

        let old = std::env::var("OPENCODE_DB").ok();
        std::env::set_var("OPENCODE_DB", custom_path.to_str().unwrap());

        let result = opencode_db_path().unwrap();
        assert_eq!(result, custom_path);

        match old {
            Some(v) => std::env::set_var("OPENCODE_DB", v),
            None => std::env::remove_var("OPENCODE_DB"),
        }
    }

    #[test]
    #[serial]
    fn test_opencode_db_path_respects_opencode_db_env_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join(".local").join("share").join("opencode");
        std::fs::create_dir_all(&data_dir).unwrap();

        let old_db = std::env::var("OPENCODE_DB").ok();
        let old_home = std::env::var("HOME").ok();
        let old_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("OPENCODE_DB", "custom.db");
        std::env::set_var("HOME", tmp.path().to_str().unwrap());
        std::env::remove_var("XDG_DATA_HOME");

        let result = opencode_db_path().unwrap();
        assert_eq!(result, data_dir.join("custom.db"));

        match old_db {
            Some(v) => std::env::set_var("OPENCODE_DB", v),
            None => std::env::remove_var("OPENCODE_DB"),
        }
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", v);
        }
    }

    #[test]
    #[serial]
    fn test_opencode_db_path_memory_returns_error() {
        let old = std::env::var("OPENCODE_DB").ok();
        std::env::set_var("OPENCODE_DB", ":memory:");

        let result = opencode_db_path();
        assert!(result.is_err());

        match old {
            Some(v) => std::env::set_var("OPENCODE_DB", v),
            None => std::env::remove_var("OPENCODE_DB"),
        }
    }

    #[test]
    #[serial]
    fn test_opencode_db_path_finds_channel_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("opencode");
        std::fs::create_dir_all(&data_dir).unwrap();
        let channel_db = data_dir.join("opencode-dev.db");
        std::fs::write(&channel_db, "").unwrap();

        let old_db = std::env::var("OPENCODE_DB").ok();
        let old_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::remove_var("OPENCODE_DB");
        std::env::set_var("XDG_DATA_HOME", tmp.path().to_str().unwrap());

        let result = opencode_db_path().unwrap();
        assert_eq!(result, channel_db);

        if let Some(v) = old_db {
            std::env::set_var("OPENCODE_DB", v);
        }
        match old_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    #[serial]
    fn test_opencode_db_path_picks_most_recent_when_both_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("opencode");
        std::fs::create_dir_all(&data_dir).unwrap();

        let standard = data_dir.join("opencode.db");
        let channel = data_dir.join("opencode-dev.db");
        std::fs::write(&standard, "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&channel, "").unwrap();

        let old_db = std::env::var("OPENCODE_DB").ok();
        let old_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::remove_var("OPENCODE_DB");
        std::env::set_var("XDG_DATA_HOME", tmp.path().to_str().unwrap());

        let result = opencode_db_path().unwrap();
        assert_eq!(result, channel, "should pick the most recently modified DB");

        if let Some(v) = old_db {
            std::env::set_var("OPENCODE_DB", v);
        }
        match old_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn test_select_claude_session_in_container_anchor_at_position_zero() {
        let uuid_a = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_b = "11111111-2222-3333-4444-555555555555";
        let stdout = format!("{uuid_a}\n{uuid_b}\n");
        let id =
            select_claude_session_in_container(stdout.as_bytes(), &HashSet::new(), Some(uuid_a))
                .unwrap();
        assert_eq!(id, uuid_a);
    }

    #[test]
    fn test_select_claude_session_in_container_active_newer_wins_over_anchor() {
        let uuid_anchor = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_newer = "11111111-2222-3333-4444-555555555555";
        let stdout = format!("{uuid_newer}\n{uuid_anchor}\n");
        let id = select_claude_session_in_container(
            stdout.as_bytes(),
            &HashSet::new(),
            Some(uuid_anchor),
        )
        .unwrap();
        assert_eq!(id, uuid_newer);
    }

    #[test]
    fn test_select_claude_session_in_container_excluded_newest_falls_to_anchor() {
        let uuid_anchor = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_sibling = "11111111-2222-3333-4444-555555555555";
        let stdout = format!("{uuid_sibling}\n{uuid_anchor}\n");
        let exclusion: HashSet<String> = std::iter::once(uuid_sibling.to_string()).collect();
        let id =
            select_claude_session_in_container(stdout.as_bytes(), &exclusion, Some(uuid_anchor))
                .unwrap();
        assert_eq!(id, uuid_anchor);
    }

    #[test]
    fn test_select_claude_session_in_container_no_candidates_errors() {
        let result = select_claude_session_in_container(b"", &HashSet::new(), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_claude_session_in_container_all_candidates_excluded_errors() {
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let exclusion: HashSet<String> = std::iter::once(uuid.to_string()).collect();
        let stdout = format!("{uuid}\n");
        let result = select_claude_session_in_container(stdout.as_bytes(), &exclusion, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_claude_session_in_container_no_anchor_picks_first_unexcluded() {
        let uuid_a = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_b = "11111111-2222-3333-4444-555555555555";
        let exclusion: HashSet<String> = std::iter::once(uuid_a.to_string()).collect();
        let stdout = format!("{uuid_a}\n{uuid_b}\n");
        let id = select_claude_session_in_container(stdout.as_bytes(), &exclusion, None).unwrap();
        assert_eq!(id, uuid_b);
    }

    #[test]
    fn test_select_claude_session_in_container_ignores_non_uuid_lines() {
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let stdout = format!("\n  \nnot-a-uuid\n{uuid}\nstill-not-a-uuid\n");
        let id =
            select_claude_session_in_container(stdout.as_bytes(), &HashSet::new(), None).unwrap();
        assert_eq!(id, uuid);
    }

    #[test]
    #[serial]
    fn test_claude_poll_fn_reads_hook_sidecar_first() {
        use std::os::unix::fs::PermissionsExt;
        let hook_tmp = tempfile::tempdir().unwrap();
        let hook_base = hook_tmp.path().join("aoe-hooks");
        std::fs::create_dir(&hook_base).unwrap();
        std::fs::set_permissions(&hook_base, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::hooks::override_base_for_test(hook_base.clone());
        crate::hooks::reset_for_test();
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::hooks::clear_base_override_for_test();
                crate::hooks::reset_for_test();
            }
        }
        let _cleanup = Cleanup;

        let instance_id = "test_sidecar_first_path";
        let hook_dir = hook_base.join(instance_id);
        std::fs::create_dir(&hook_dir).unwrap();
        std::fs::set_permissions(&hook_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sidecar_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        std::fs::write(hook_dir.join("session_id"), sidecar_uuid).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let disk_uuid = "11111111-2222-3333-4444-555555555555";
        std::fs::write(project_dir.join(format!("{disk_uuid}.jsonl")), "d\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let poll = claude_poll_fn(
            "/tmp/myproject".to_string(),
            None,
            instance_id.to_string(),
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some(sidecar_uuid));

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_claude_poll_fn_skips_stale_sidecar_falls_through_to_disk() {
        use std::os::unix::fs::PermissionsExt;
        let hook_tmp = tempfile::tempdir().unwrap();
        let hook_base = hook_tmp.path().join("aoe-hooks");
        std::fs::create_dir(&hook_base).unwrap();
        std::fs::set_permissions(&hook_base, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::hooks::override_base_for_test(hook_base.clone());
        crate::hooks::reset_for_test();
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::hooks::clear_base_override_for_test();
                crate::hooks::reset_for_test();
            }
        }
        let _cleanup = Cleanup;

        let instance_id = "test_sidecar_stale_falls_through";
        let hook_dir = hook_base.join(instance_id);
        std::fs::create_dir(&hook_dir).unwrap();
        std::fs::set_permissions(&hook_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stale_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let sidecar_path = hook_dir.join("session_id");
        std::fs::write(&sidecar_path, stale_uuid).unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(10 * 60);
        std::fs::File::options()
            .write(true)
            .open(&sidecar_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let disk_uuid = "11111111-2222-3333-4444-555555555555";
        std::fs::write(project_dir.join(format!("{disk_uuid}.jsonl")), "d\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        let poll = claude_poll_fn(
            "/tmp/myproject".to_string(),
            None,
            instance_id.to_string(),
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some(disk_uuid));

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_capture_claude_session_active_newer_wins_over_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let uuid_anchor = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let uuid_active = "11111111-2222-3333-4444-555555555555";

        std::fs::write(project_dir.join(format!("{uuid_anchor}.jsonl")), "k\n").unwrap();
        let anchor_time = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(project_dir.join(format!("{uuid_anchor}.jsonl")))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(anchor_time))
            .unwrap();
        std::fs::write(project_dir.join(format!("{uuid_active}.jsonl")), "a\n").unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_anchor), &HashSet::new())
                .unwrap(),
            uuid_active
        );

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }
}

//! Session ID capture logic for all supported agent types.

use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use uuid::Uuid;
mod omp;

pub(crate) use omp::*;

/// Iterate directory entries, silently skipping unreadable ones.
///
/// Wraps `std::fs::read_dir` and filters out individual entry errors (e.g.
/// transient permission failures) so that one bad entry doesn't abort the
/// entire directory scan.
///
/// This filters `read_dir`'s per-entry `Err` only, which is a much narrower
/// guarantee than it sounds. Nothing here stats the entry, so a dangling
/// symlink, a symlink cycle, a directory, or a FIFO is yielded as an ordinary
/// entry. Callers that need a real file behind the name have to check for
/// themselves; see `scan_claude_project_dir`.
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

/// Resolve the Claude config dir the *launched pane* will see.
///
/// The launch path injects the session's profile-scoped `environment` entries
/// into the pane, so a profile pinning `CLAUDE_CONFIG_DIR` makes the agent read
/// and write a config tree that is not `~/.claude`. Every host-side read of
/// Claude's on-disk state has to resolve the same way or it inspects a tree the
/// agent never touches: the transcript probe then reports a real conversation
/// absent, and the project-dir scan can hand back a conversation belonging to
/// another profile that happens to share the cwd. See #3399.
///
/// Precedence mirrors [`crate::hooks::agent_settings_path_in`]: the session's
/// host environment first, then AoE's own env (a var exported in the shell that
/// launched `aoe` is inherited by the agent too), then `~/.claude`.
fn claude_home_for_host_environment(host_env: &[String]) -> Result<PathBuf> {
    match claude_config_dir_override(host_env) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => resolve_agent_home(None, ".claude"),
    }
}

/// The `CLAUDE_CONFIG_DIR` value the launched pane will see, if any: the
/// session's host environment wins, then AoE's own env. Empty is unset.
///
/// Shares [`crate::hooks::resolve_config_dir_override`] with the hook-install
/// path so the read side and the write side cannot drift on precedence.
fn claude_config_dir_override(host_env: &[String]) -> Option<String> {
    crate::hooks::resolve_config_dir_override("CLAUDE_CONFIG_DIR", host_env)
}

/// Resolve a path to a comparable identity: canonicalize when the directory
/// exists, otherwise fall back to lexical `.`/`..` normalization so a
/// historical unnormalized spelling (a pre-#2858 worktree `project_path` like
/// `/repos/x/../x-worktrees/b`) still compares equal to the plain spelling
/// after the directory has been deleted.
pub(crate) fn canonicalize_or_raw(path: &str) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| crate::git::template::lexical_normalize(Path::new(path)))
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

/// Generate a new UUID v4 to pin an agent session id at launch. Claude
/// (`--session-id`), its fork children, and Pi (`--session-id`) all accept
/// this spelling.
pub(crate) fn generate_session_uuid() -> String {
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
/// falling back to `.claude.json` if the dir scan result is stale.
///
/// `host_env` is the session's profile-scoped host environment, which selects
/// the config tree both reads resolve against (see
/// [`claude_home_for_host_environment`]).
///
/// Used as a fallback when hooks don't fire (e.g. after `/clear` or `/new`).
/// Both arms only ever yield an id with a transcript on disk: the dir scan by
/// construction, and the `.claude.json` arm because it is gated on one. A
/// freshly cleared thread is therefore declined until its first content lands,
/// which is the one case this narrows.
pub(crate) fn capture_claude_session_id(
    project_path: &str,
    known_session_id: Option<&str>,
    exclusion: &HashSet<String>,
    host_env: &[String],
) -> Result<String> {
    let claude_home = claude_home_for_host_environment(host_env)?;
    let canonical = canonicalize_or_raw(project_path);

    if let Some((id, modified)) =
        scan_claude_project_dir(&claude_home, &canonical, known_session_id, exclusion)?
    {
        let age = modified.elapsed().unwrap_or(Duration::from_secs(u64::MAX));
        if age <= Duration::from_secs(5 * 60) {
            return Ok(id);
        }
    }

    let claude_json_path = claude_json_path(&claude_home, host_env);
    if let Some(id) = read_claude_json_session_id(&claude_json_path, &canonical) {
        if exclusion.contains(&id) {
            return Err(anyhow::anyhow!(
                "claude.json lastSessionId {} is excluded (claimed by another instance)",
                id
            ));
        }
        let claude_json = std::fs::metadata(&claude_json_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let is_fresh = claude_json
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age <= Duration::from_secs(5 * 60));
        // `is_fresh` is the mtime of `.claude.json` itself, which any live
        // Claude anywhere rewrites, so it says nothing about when *this*
        // directory's `lastSessionId` was set and a value months old still
        // reads as fresh. And unlike the dir scan above, which can only return
        // ids it found as files, this slot can name a UUID no transcript was
        // ever written for. Requiring the conversation to exist is what keeps
        // `--resume` off an id Claude answers with "No conversation found",
        // which leaves the pane dead on every restart from then on.
        //
        // The cost is the short window after `/clear` or `/new` where the slot
        // names a thread Claude has not written to yet: the arm declines it,
        // and the dir scan picks the new id up once content lands. Resuming it
        // in that window would fail anyway.
        if is_fresh
            && Uuid::parse_str(&id).is_ok()
            && !claude_host_transcript_confirmed_absent(&canonical.to_string_lossy(), &id, host_env)
        {
            return Ok(id);
        }
    }

    anyhow::bail!("No active Claude session found for {}", project_path)
}

/// Whether we can affirmatively prove Claude has *no* persisted transcript for
/// `session_id` under `project_path` on the host filesystem.
///
/// Claude only writes `<config>/projects/<encoded-cwd>/<uuid>.jsonl` once a
/// conversation has real content. A session AoE minted a UUID for but that was
/// killed before the first prompt (an "empty thread") therefore has a stored
/// `agent_session_id` that never hit disk, and `claude --resume <uuid>` on it
/// fails with "No conversation found" every time. Callers use this to launch
/// such an id as a fresh pinned session (`--session-id <uuid>`) instead of a
/// guaranteed-to-fail `--resume`.
///
/// `<config>` is resolved from the session's profile-scoped `host_env`, the
/// same way the launch path resolves it (see
/// [`claude_home_for_host_environment`]). Probing the default `~/.claude` for a
/// profile pinned elsewhere would report every real conversation absent and
/// downgrade it to `--session-id <uuid>`, which the agent rejects as already in
/// use, killing the pane outright. See #3399.
///
/// Returns `true` ONLY when the Claude home resolves and the transcript file is
/// confirmed missing. Any uncertainty (home dir unresolved) returns `false` so
/// the caller preserves the existing `--resume` attempt rather than risk
/// downgrading a real conversation to a fresh start. The check is
/// existence-only (no mtime freshness gate), so an idle-but-real conversation
/// whose jsonl is older than the live-capture window is still reported present.
pub(crate) fn claude_host_transcript_confirmed_absent(
    project_path: &str,
    session_id: &str,
    host_env: &[String],
) -> bool {
    let Ok(claude_home) = claude_home_for_host_environment(host_env) else {
        return false;
    };
    let canonical = canonicalize_or_raw(project_path);
    let dir_name = encode_claude_project_path(&canonical.to_string_lossy());
    let transcript = claude_home
        .join("projects")
        .join(dir_name)
        .join(format!("{session_id}.jsonl"));
    !transcript.is_file()
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

        // `fs::metadata` rather than `DirEntry::metadata`/`file_type`, which
        // describe the link rather than its target. A symlinked transcript has
        // to keep counting (#3399's workaround tells users to make one), and
        // following the link reports the target's mtime, the one that advances
        // as Claude appends. A directory, FIFO or dangling link named
        // `<uuid>.jsonl` is otherwise handed back as a resume id with no
        // transcript behind it.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

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

/// Where Claude keeps `.claude.json` for the config tree at `claude_home`.
///
/// It sits *inside* the dir when `CLAUDE_CONFIG_DIR` selects it, but next to
/// (not inside) the default `~/.claude`, so the default case cannot be derived
/// from `claude_home` alone.
fn claude_json_path(claude_home: &Path, host_env: &[String]) -> PathBuf {
    if claude_config_dir_override(host_env).is_some() {
        claude_home.join(".claude.json")
    } else {
        claude_home.with_file_name(".claude.json")
    }
}

/// Read `lastSessionId` from `.claude.json` for a given project path.
fn read_claude_json_session_id(claude_json: &Path, project_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(claude_json).ok()?;
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
/// 2. Otherwise scan `<config>/projects/<encoded-path>/`, where `<config>` is
///    the session's profile-scoped Claude config dir (`host_env`), so a
///    same-cwd peer in another profile is never a candidate. The scan uses
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
    host_env: Vec<String>,
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
            &host_env,
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

/// The `sh` snippet shipped to `docker exec` to list candidate transcripts.
///
/// All three checks dereference: `[ -f ]` for type, `find -L` for freshness,
/// `ls -tL` for ordering. A symlink's own mtime is frozen at creation, so a
/// link left undereferenced ages out of the five-minute gate while its target
/// is still being appended. The host scan reads through links for the same
/// reason (#3454), and the two have to agree for
/// `claude_host_transcript_confirmed_absent` to mean anything.
///
/// `docker exec` inherits the container's environment, not the pane's, so
/// `$CLAUDE_CONFIG_DIR` here is the value AoE injected at `docker run`. A
/// wrapper that exports its own before `exec claude` writes where this never
/// looked, and the session captures nothing (#3638). A declared
/// `agent_config_dir` cannot stand in for it: the user mounts that directory
/// themselves and AoE never sees its container path. So each distinct
/// `CLAUDE_CONFIG_DIR` a live process declares is tried first, the first
/// directory holding fresh transcripts winning, and the injected value is the
/// fallback. The `/proc` walk costs a pipeline per process, which is bounded
/// by a session container holding one agent and its children.
fn claude_container_list_snippet(dir_name: &str) -> String {
    format!(
        r#"
# A process can exit between the glob and the read; its redirect error is
# noise, and nothing here reports through stderr.
exec 2>/dev/null
list_fresh() {{
  DIR="$1/projects/{dir_name}"
  [ -d "$DIR" ] || return 1
  missed=1
  # Split the listing on newlines alone. The dir now comes from whatever a
  # container process declares, so it can hold a space, and the default IFS
  # would shred the path into arguments no `[ -f ]` matches.
  OIFS=$IFS
  IFS='
'
  for f in $(ls -tL "$DIR"/*.jsonl 2>/dev/null); do
    [ -f "$f" ] || continue
    [ -n "$(find -L "$f" -mmin -5 2>/dev/null)" ] || continue
    basename "$f" .jsonl
    missed=0
  done
  IFS=$OIFS
  return $missed
}}
FALLBACK="${{CLAUDE_CONFIG_DIR:-$HOME/.claude}}"
SEEN=""
for e in /proc/[0-9]*/environ; do
  [ -r "$e" ] || continue
  d=$(tr '\0' '\n' < "$e" | sed -n 's/^CLAUDE_CONFIG_DIR=//p' | head -n 1)
  [ -n "$d" ] || continue
  [ "$d" = "$FALLBACK" ] && continue
  case "$SEEN" in *"|$d|"*) continue ;; esac
  SEEN="$SEEN|$d|"
  list_fresh "$d" && exit 0
done
list_fresh "$FALLBACK"
exit 0
"#
    )
}

/// Capture Claude Code session ID inside a Docker container.
///
/// Lists every fresh (≤ 5 min mtime) UUID-named jsonl in the Claude config
/// dir's `projects/<encoded-cwd>/` newest-first via `docker exec` (see
/// [`claude_container_list_snippet`] for which dir that is), wrapped in
/// [`run_with_timeout`] (5 s) so a hung exec
/// cannot block the poller thread, then delegates per-pane attribution to
/// [`select_claude_session_in_container`].
pub(crate) fn capture_claude_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
    exclusion: &HashSet<String>,
    known_session_id: Option<&str>,
) -> Result<String> {
    let snippet = claude_container_list_snippet(&encode_claude_project_path(container_cwd));

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

/// Number of leading lines and bytes scanned when locating a pi-family
/// session header. The byte cap matters because `BufRead::lines` otherwise
/// allocates without bound for one hostile or corrupt line.
const PI_HEADER_SCAN_LINES: usize = 8;
const PI_HEADER_SCAN_BYTES: usize = 64 * 1024;

fn extract_pi_header_fields(path: &Path) -> Option<(Option<String>, Option<String>)> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut reader = std::io::BufReader::new(file);
    let mut consumed = 0usize;
    for _ in 0..PI_HEADER_SCAN_LINES {
        let mut line = String::new();
        let mut limited =
            (&mut reader).take((PI_HEADER_SCAN_BYTES.saturating_sub(consumed) + 1) as u64);
        let read = std::io::BufRead::read_line(&mut limited, &mut line).ok()?;
        if read == 0 {
            return None;
        }
        consumed = consumed.saturating_add(read);
        if consumed > PI_HEADER_SCAN_BYTES {
            return None;
        }
        if let Some(header) = parse_pi_header_json(&line) {
            return Some(header);
        }
    }
    None
}

/// Parse a single already-in-memory `.jsonl` line into a pi-family session
/// header's `(id, cwd)`, returning `None` unless the record's `"type"` is
/// `"session"`.
///
/// Non-session and malformed lines yield `None`, so bounded scanners can keep
/// the first matching record.
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

pub(crate) fn extract_pi_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let uuid_part = stem.rsplit('_').next()?;
    Uuid::parse_str(uuid_part).ok()?;
    Some(uuid_part.to_string())
}

#[cfg(test)]
pub(crate) fn extract_pi_cwd_from_header(path: &Path) -> Option<String> {
    extract_pi_header_fields(path).and_then(|(_, cwd)| cwd)
}

/// Polling closure over the sidecar Pi's AoE extension writes: the pane's own
/// conversation, `/new` included, with no store scan involved.
/// Polling closure over the sidecar Pi's AoE extension writes.
///
/// The source says where the pane publishes: a container's bind-backed
/// directory or the per-instance hook dir. Getting it wrong is silent, the
/// poller simply never observing anything, so it is passed in rather than
/// re-derived here.
pub(crate) fn pi_sidecar_poll_fn(
    instance_id: String,
    source: crate::session::instance::PiSidecarSource,
) -> impl Fn() -> Option<crate::session::poller::SessionIdObservation> + Send + 'static {
    move || {
        use crate::session::instance::PiSidecarSource;
        let id = match source {
            PiSidecarSource::SandboxDir(ref dir) => std::fs::read_to_string(dir.join("session_id"))
                .ok()
                .map(|raw| raw.trim().to_string())
                .filter(|id| Uuid::parse_str(id).is_ok()),
            PiSidecarSource::HostHooks => crate::hooks::read_hook_session_id(&instance_id),
        };
        id.and_then(validated_session_id)
            .map(crate::session::poller::SessionIdObservation::instance_sidecar)
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
    compose_exclusion_in(
        current_instance_id,
        extra,
        &crate::tmux::LiveSessionSnapshot::new(),
    )
}

/// [`compose_exclusion`] against a snapshot the caller already holds, so a
/// pass that also probes per-instance liveness observes tmux once instead of
/// twice.
fn compose_exclusion_in(
    current_instance_id: &str,
    extra: &HashSet<String>,
    live: &crate::tmux::LiveSessionSnapshot,
) -> HashSet<String> {
    let mut set = build_exclusion_set(current_instance_id, live);
    set.extend(extra.iter().cloned());
    set
}

/// Extend [`compose_exclusion`] with conversations same-project peers parked
/// for `current_capture_agent` during an engine swap. When
/// `include_inactive_same_tool` is true, also include the sids of stopped,
/// archived, or pane-less peers whose own built-in agent resolves to
/// `current_capture_agent`, so a wrapper and the agent it wraps count as one
/// store. Persisted peers are read from `sessions.json` via `Storage` for the
/// caller's effective profile.
///
/// Used by `Instance::try_retroactive_capture` and snapshotted when its poller
/// starts. Parked conversations are no longer published in the peer's tmux
/// environment, so [`build_exclusion_set`] cannot see them. Without this set,
/// another session can capture the parked conversation before its owner swaps
/// back. Claude, host Codex, and host Kimi additionally need inactive
/// same-tool protection because their shared-store MRU scans can select a
/// conversation after the owning pane disappears. Host Pi is included for its
/// poller alone: acquisition no longer scans its store at all (#3576), but a
/// peer that goes inactive after this pane launched can still leave the
/// freshest file inside the poller's floor. Sandboxed Codex, Kimi, and Pi omit
/// the protection because their stores are instance-private or are not
/// captured from the host (#3317).
///
/// Scope: host stores are keyed by each agent's effective home, not by AoE
/// profile, but this helper inspects only `sessions.json` for the caller's
/// effective profile. A stopped peer in another profile against the same
/// agent home will not be excluded; callers needing global ownership must
/// compose their own cross-profile check.
pub(crate) fn compose_exclusion_with_persisted_peers(
    current_instance_id: &str,
    current_project_path: &str,
    current_capture_agent: &str,
    include_inactive_same_tool: bool,
    profile: &str,
    retroactive_capture_excludes: &HashSet<String>,
) -> HashSet<String> {
    // One observation for the whole pass. Both halves consult tmux: the
    // cross-instance scan needs the live session names, and the walk below
    // visits every stored session sharing the project path, trashed ones
    // included, so a per-instance liveness probe costs a fork each. A store of
    // a few hundred sessions made that the dominant cost of the pass.
    // `names() == None` (server unreachable) reads as "no live pane" here,
    // which is what the per-item probe already did when its own
    // `list-sessions` failed, and this pass re-runs.
    let live = crate::tmux::LiveSessionSnapshot::new();
    let mut set = compose_exclusion_in(current_instance_id, retroactive_capture_excludes, &live);
    let Ok(storage) = crate::session::storage::Storage::new_unwatched(profile) else {
        return set;
    };
    let Ok(instances) = storage.load() else {
        return set;
    };
    // Compare canonicalized paths, not raw strings: worktree sessions created
    // from `../`-style templates historically stored an unnormalized
    // `project_path` (e.g. `/repos/x/../x-worktrees/b`), and a raw comparison
    // silently drops them from this exclusion even though they share the
    // directory — re-opening the #2355 steal for exactly those peers (#2858).
    let canonical_current = canonicalize_or_raw(current_project_path);
    for inst in instances {
        if inst.id == current_instance_id {
            continue;
        }
        if canonicalize_or_raw(&inst.project_path) != canonical_current {
            continue;
        }
        // A peer that swapped away still owns the conversation it parked and
        // intends to resume it on a swap back. It is excluded regardless of the
        // peer's current tool or liveness: its pane is running another engine,
        // so the live tmux ownership scan cannot discover this id.
        //
        // `swap_tool` keys this map on the raw outgoing tool, so an exact
        // lookup makes a conversation parked under `claude-personal` invisible
        // to `claude` and to every other alias of it, and the MRU scan then
        // adopts what the peer means to resume. Match on the built-in each key
        // resolves to instead, which only ever widens the set (#3638).
        for (parked_tool, parked) in &inst.prior_tool_session_ids {
            let parked_agent =
                crate::session::instance::resolved_agent_for(&inst.source_profile, parked_tool, "")
                    .map_or(parked_tool.as_str(), |a| a.name);
            if parked_agent != current_capture_agent {
                continue;
            }
            if let Some(sid) = parked.agent_session_id.as_deref().filter(|s| !s.is_empty()) {
                set.insert(sid.to_string());
            }
        }
        // Matched on the built-in each side resolves to, not on the raw
        // tool: a wrapper and the agent it wraps share one store, so a raw
        // compare leaves an inactive base-agent peer's conversation open to
        // the wrapper's mtime scan (#3638).
        let peer_capture_agent = inst.capture_agent_name().unwrap_or(inst.tool.as_str());
        if !include_inactive_same_tool || peer_capture_agent != current_capture_agent {
            continue;
        }
        let should_exclude = matches!(inst.status, crate::session::Status::Stopped)
            || inst.is_archived()
            || !inst.has_live_tmux_pane_in(&live);
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
fn build_exclusion_set(
    current_instance_id: &str,
    live: &crate::tmux::LiveSessionSnapshot,
) -> HashSet<String> {
    let Some(names) = live.names() else {
        return HashSet::new();
    };

    let aoe_sessions: Vec<&str> = names
        .iter()
        .map(String::as_str)
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
fn run_with_timeout(cmd: std::process::Command, timeout: Duration, label: &str) -> Result<Vec<u8>> {
    run_with_timeout_inner(cmd, timeout, label, None)
}

pub(super) fn run_with_timeout_limit(
    cmd: std::process::Command,
    timeout: Duration,
    label: &str,
    max_stdout_bytes: usize,
) -> Result<Vec<u8>> {
    run_with_timeout_inner(cmd, timeout, label, Some(max_stdout_bytes))
}

fn run_with_timeout_inner(
    mut cmd: std::process::Command,
    timeout: Duration,
    label: &str,
    max_stdout_bytes: Option<usize>,
) -> Result<Vec<u8>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", label))?;

    let stdout_pipe = child.stdout.take();
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let buf = stdout_pipe.map(|mut reader| {
            let mut buf = Vec::new();
            if let Some(limit) = max_stdout_bytes {
                reader
                    .take(limit.saturating_add(1) as u64)
                    .read_to_end(&mut buf)
                    .ok();
            } else {
                reader.read_to_end(&mut buf).ok();
            }
            buf
        });
        let _ = stdout_tx.send(buf);
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!("{} timed out", label));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(anyhow::anyhow!("Failed to wait on {}: {}", label, error));
            }
        }
    };

    // The child exited, but a grandchild that inherited the stdout write end
    // (a backgrounded helper the command spawned) keeps `read_to_end` blocking
    // even though the child is gone. Bound the drain by the remaining deadline
    // so the timeout guarantee holds on the success path too, not just on the
    // kill path; mirrors `process::run_with_timeout`. When the try_wait loop
    // already burned the budget, `remaining` is zero and recv_timeout returns an
    // empty buffer at once: intended fail-open, never a block. The reader thread
    // is deliberately detached; it exits once the grandchild closes the fd, so
    // the leak is bounded by the grandchild's lifetime. Joining it would
    // reintroduce the unbounded block the timeout exists to prevent.
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let stdout_bytes = stdout_rx
        .recv_timeout(remaining)
        .ok()
        .flatten()
        .unwrap_or_default();
    if max_stdout_bytes.is_some_and(|limit| stdout_bytes.len() > limit) {
        anyhow::bail!("{} exceeded its stdout limit", label);
    }
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

/// Total wall-clock budget for the whole preassign dance (serve boot + POST).
/// opencode's headless server boots in ~1.8s measured; 6s leaves slack on a
/// loaded machine while keeping the opt-in launch stall bounded before we give
/// up and let the poller take over.
const OPENCODE_PREASSIGN_DEADLINE: Duration = Duration::from_secs(6);

/// RAII guard that force-reaps an ephemeral `opencode serve` child, and its
/// whole process group, on drop. Guarantees a preassign attempt that returns
/// early, errors, or unwinds never leaks a headless server holding a port.
/// The successful POST is the DB commit boundary, so tearing the server down
/// here (before the caller launches `opencode --session <id>`) also avoids two
/// servers touching opencode's SQLite store at once.
struct ServeGuard(Option<std::process::Child>);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        terminate_serve_group(child.id());
        std::thread::sleep(Duration::from_millis(150));
        if matches!(child.try_wait(), Ok(None)) {
            kill_serve_group(child.id());
        }
        let _ = child.wait();
    }
}

/// Signal the ephemeral `opencode serve` process group (the child was spawned
/// with `process_group(0)`), then the bare pid as a fallback. No-op off unix.
#[cfg(unix)]
fn signal_serve_group(pid: u32, sig: nix::sys::signal::Signal) {
    use nix::sys::signal::{kill, killpg};
    use nix::unistd::Pid;
    let p = Pid::from_raw(pid as i32);
    let _ = killpg(p, sig);
    let _ = kill(p, sig);
}

fn terminate_serve_group(pid: u32) {
    #[cfg(unix)]
    signal_serve_group(pid, nix::sys::signal::Signal::SIGTERM);
    #[cfg(not(unix))]
    let _ = pid;
}

fn kill_serve_group(pid: u32) {
    #[cfg(unix)]
    signal_serve_group(pid, nix::sys::signal::Signal::SIGKILL);
    #[cfg(not(unix))]
    let _ = pid;
}

/// Pre-assign an OpenCode session id before launch, eliminating the post-launch
/// SQLite capture race by creating the session up front.
///
/// Spawns a throwaway `opencode serve` on a loopback port, `POST`s a chosen
/// `ses_` id bound to `project_path` via `POST /api/session`, then tears the
/// server down. The subsequent `opencode --session <id>` launch resumes the
/// pre-created (empty) session. Opt-in and fail-closed: any failure returns
/// `None`, and the caller falls back to the existing background poller.
///
/// Host sessions only: the loopback server is unreachable from inside a sandbox
/// container, so sandboxed opencode keeps polling.
pub(crate) fn preassign_opencode_session_id(project_path: &str) -> Option<String> {
    preassign_opencode_session_id_impl(project_path)
        .map_err(|e| {
            tracing::warn!(
                target: "session.capture",
                "opencode session preassign failed ({e}); falling back to the SQLite poller"
            )
        })
        .ok()
        .and_then(validated_session_id)
}

fn preassign_opencode_session_id_impl(project_path: &str) -> Result<String> {
    // Reserve a free loopback port from the OS, then release it so the spawned
    // server can bind it. The tiny bind/drop/bind race is covered by the
    // readiness timeout and the caller's safe fallback.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .context("failed to reserve a loopback port for opencode serve")?
        .local_addr()
        .context("failed to read the reserved loopback port")?
        .port();

    let id = format!("ses_{}", Uuid::new_v4().simple());

    let mut cmd = std::process::Command::new("opencode");
    cmd.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ])
    .current_dir(project_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    // Own process group so ServeGuard can reap `opencode serve` and any workers
    // it spawns, not just the immediate child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .context("failed to spawn `opencode serve` for preassign")?;
    let _guard = ServeGuard(Some(child));

    let base = format!("http://127.0.0.1:{port}");
    // `acquire_session_id` runs on a launch thread that may itself be async: on
    // the CLI it runs under the `#[tokio::main]` entrypoint, i.e. *inside* a live
    // Tokio runtime. Building a runtime and `block_on`-ing it on that same thread
    // panics with "Cannot start a runtime from within a runtime". Run the
    // short-lived current-thread runtime on a dedicated OS thread instead, which
    // never carries an ambient runtime, so `block_on` is valid regardless of
    // whether the caller (CLI, a server `spawn_blocking` worker, or the TUI event
    // loop) is itself async. `thread::scope` lets the worker borrow
    // `id`/`base`/`project_path` without `'static` clones and keeps the
    // `opencode serve` `_guard` alive across the join.
    let preassign = || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build the preassign runtime")?;

        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .context("failed to build the preassign HTTP client")?;

            let deadline = Instant::now() + OPENCODE_PREASSIGN_DEADLINE;
            loop {
                if let Ok(resp) = client.get(format!("{base}/api/session")).send().await {
                    if resp.status().is_success() {
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    anyhow::bail!("opencode serve did not become ready within the deadline");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            let body = serde_json::json!({
                "id": id,
                "location": { "directory": project_path },
            });
            let resp = client
                .post(format!("{base}/api/session"))
                .json(&body)
                .send()
                .await
                .context("opencode preassign POST /api/session failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("opencode preassign POST returned {}", resp.status());
            }
            let created: serde_json::Value = resp
                .json()
                .await
                .context("opencode preassign response was not JSON")?;
            let created_id = created
                .get("data")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str());
            if created_id != Some(id.as_str()) {
                anyhow::bail!("opencode assigned {created_id:?}, expected {id}");
            }
            Ok::<(), anyhow::Error>(())
        })
    };

    std::thread::scope(|scope| {
        scope
            .spawn(preassign)
            .join()
            .map_err(|_| anyhow::anyhow!("opencode preassign worker thread panicked"))?
    })?;

    Ok(id)
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

/// Shell snippet executed via `docker exec` to enumerate Codex `.jsonl` and
/// `.jsonl.zst` session files inside the container. Each file is emitted as a
/// `===CODEX:<unix-mtime>:<basename>===` header followed by the first line of the
/// file and a `===END===` trailer.
const CODEX_CONTAINER_LIST_SCRIPT: &str = r#"SESS_DIR="${CODEX_HOME:-$HOME/.codex}/sessions"
[ -d "$SESS_DIR" ] || exit 0
find "$SESS_DIR" -type f \( -name '*.jsonl' -o -name '*.jsonl.zst' \) | while read -r f; do
  ts=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)
  bn=$(basename "$f")
  printf '===CODEX:%s:%s===\n' "$ts" "$bn"
  case "$f" in
    *.jsonl.zst) zstd -dc "$f" 2>/dev/null | head -n 1 ;;
    *) head -n 1 "$f" ;;
  esac
  printf '\n===END===\n'
done
"#;

/// Capture session ID from Codex filesystem.
///
/// Walks the Codex sessions directory (including date-partitioned `YYYY/MM/DD/` subdirectories)
/// for `.jsonl` and `.jsonl.zst` rollout files and extracts the UUID from the
/// most recent one. Codex filenames follow the pattern
/// `rollout-<timestamp>-<uuid>.jsonl[.zst]`.
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
        let reader = open_codex_rollout(path).ok()?;
        let reader = std::io::BufReader::new(reader);
        let first_line = std::io::BufRead::lines(reader).next()?.ok()?;
        let cwd = parse_codex_cwd_from_json(&first_line, &uuid)?;
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

/// Parse the CWD from a Codex rollout's first line.
///
/// The filename UUID remains authoritative because it names the rollout Codex
/// can resume. When metadata declares `session_id` or `id`, require every
/// present value to identify that same rollout. Codex child rollouts may point
/// `session_id` at their parent while using their own filename UUID; rejecting
/// that mismatch prevents the child from winning the newest-mtime scan.
/// Metadata without either id remains supported for compatibility with older
/// rollouts and capture test fixtures.
fn parse_codex_cwd_from_json(line: &str, filename_uuid: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = parsed.get("payload")?;
    let filename_id = Uuid::parse_str(filename_uuid).ok()?;
    for key in ["session_id", "id"] {
        if let Some(value) = payload.get(key) {
            let declared_id = Uuid::parse_str(value.as_str()?).ok()?;
            if declared_id != filename_id {
                return None;
            }
        }
    }

    payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

/// Extract UUID from a Codex rollout filename.
///
/// Codex filenames follow the pattern `rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl`.
/// The UUID is the last 36 characters of the stem (before `.jsonl`).
fn extract_codex_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.strip_suffix(".jsonl").unwrap_or(stem);
    if stem.len() >= 36 {
        let candidate = &stem[stem.len() - 36..];
        if Uuid::parse_str(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Returns whether `path` is a plain or zstd-compressed Codex rollout.
fn is_codex_rollout(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

/// Opens a Codex rollout, transparently decoding zstd-compressed files.
fn open_codex_rollout(path: &Path) -> Result<Box<dyn Read>> {
    let file = std::fs::File::open(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}

/// Recursively collect Codex session `.jsonl` files, descending into date-partitioned dirs.
///
/// Directories whose names are all ASCII digits (e.g. `2025`, `03`, `06`) are treated as
/// date components and recursed into. Files ending in `.jsonl` or `.jsonl.zst` are collected as
/// session entries.
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
        } else if is_codex_rollout(&path) {
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
        let cwd = match parse_codex_cwd_from_json(json_part.trim(), &uuid) {
            Some(cwd) => cwd,
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

// ─── Kimi Code session capture ────────────────────────────────────────────────

/// One live entry from Kimi's session index.
struct KimiSession {
    id: String,
    session_dir: String,
    work_dir: String,
}

/// Parse Kimi's append-only session index (`session_index.jsonl`) into the set
/// of live sessions. Each line is a JSON object: either a session record
/// (`{sessionId, sessionDir, workDir}`) or a deletion tombstone
/// (`{sessionId, deleted: true}`). Later lines win, and a tombstone removes an
/// earlier record, mirroring Kimi's own `readSessionIndex`. Malformed lines are
/// skipped rather than failing the whole read.
fn read_kimi_session_index(index_path: &Path) -> Result<Vec<KimiSession>> {
    if !index_path.exists() {
        anyhow::bail!("Kimi session index not found at {}", index_path.display());
    }
    let content = std::fs::read_to_string(index_path)
        .with_context(|| format!("Failed to read {}", index_path.display()))?;

    let mut live: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session_id) = value.get("sessionId").and_then(|v| v.as_str()) else {
            continue;
        };
        if value.get("deleted").and_then(|v| v.as_bool()) == Some(true) {
            live.remove(session_id);
            continue;
        }
        let (Some(session_dir), Some(work_dir)) = (
            value.get("sessionDir").and_then(|v| v.as_str()),
            value.get("workDir").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        live.insert(
            session_id.to_string(),
            (session_dir.to_string(), work_dir.to_string()),
        );
    }

    Ok(live
        .into_iter()
        .map(|(id, (session_dir, work_dir))| KimiSession {
            id,
            session_dir,
            work_dir,
        })
        .collect())
}

/// Slack (ms) applied to the launch-time floor to absorb filesystem mtimes
/// that are only second-granular: a session directory created in the same
/// second AoE launched can carry an mtime a few hundred ms below the
/// millisecond launch timestamp, and must still count as "created after
/// launch". Far smaller than the gap to any genuinely historical session.
const KIMI_MTIME_FLOOR_SLACK_MS: f64 = 2000.0;

/// Unix mtime (milliseconds) of a session directory, `0` when it cannot be
/// read. Kimi creates a fresh `sessionDir` per session (appends inside it do
/// not touch the directory mtime), so a directory newer than the launch floor
/// is the session created for the current run.
fn kimi_session_dir_mtime_ms(session_dir: &str) -> u64 {
    std::fs::metadata(session_dir)
        .and_then(|m| m.modified())
        .map(crate::util::system_time_to_ms)
        .unwrap_or(0)
}

/// Pick the newest unexcluded Kimi session whose `workDir` matches
/// `project_path`. Paths are canonicalized so a symlinked or `/tmp` ->
/// `/private/tmp` cwd still matches.
///
/// When `launch_time_ms` is `Some`, only sessions whose directory was created
/// at/after that floor are eligible, so a fresh live poll cannot latch onto a
/// pre-existing conversation for the same `workDir` before Kimi writes the new
/// record. Retroactive recovery passes `None` to allow resuming an older
/// session.
fn select_kimi_session(
    sessions: &[KimiSession],
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let canonical_match = canonicalize_or_raw(project_path);
    let mut candidates: Vec<(String, u64)> = sessions
        .iter()
        .filter(|s| !exclusion.contains(&s.id))
        .filter(|s| canonicalize_or_raw(&s.work_dir) == canonical_match)
        .map(|s| (s.id.clone(), kimi_session_dir_mtime_ms(&s.session_dir)))
        .collect();

    if let Some(threshold) = launch_time_ms {
        candidates
            .retain(|(_, mtime_ms)| (*mtime_ms as f64) + KIMI_MTIME_FLOOR_SLACK_MS >= threshold);
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
    candidates
        .into_iter()
        .next()
        .map(|(id, _)| id)
        .ok_or_else(|| anyhow::anyhow!("No Kimi session found matching project path"))
}

/// Capture a Kimi Code session ID for `project_path`.
///
/// Reads `session_index.jsonl` under the Kimi home resolved from
/// `environment` and returns the id of the most recently created session
/// whose recorded `workDir` matches `project_path`, skipping any ids in
/// `exclusion`. `environment` must be the launched pane's host environment so
/// the scan reads the same physical store Kimi writes (`KIMI_CODE_HOME`
/// honored through launch's `$VAR` / bare-key grammar). `launch_time_ms`
/// gates live polling to sessions created after this run started (`None` for
/// retroactive recovery). Kimi resumes the returned id with `kimi --session
/// <id>`.
pub(crate) fn capture_kimi_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
    environment: &[String],
) -> Result<String> {
    let home = kimi_home_for_environment(environment)
        .ok_or_else(|| anyhow::anyhow!("could not resolve the Kimi home"))?;
    let sessions = read_kimi_session_index(&home.join("session_index.jsonl"))?;
    select_kimi_session(&sessions, project_path, exclusion, launch_time_ms)
}

/// Polling closure for Kimi Code session tracking. `launch_time_ms` floors the
/// live poll so it never claims a conversation that predates this launch.
/// `environment` is snapshotted from the instance at poller construction so
/// every tick reads the store the launched pane writes.
pub(crate) fn kimi_poll_fn(
    project_path: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
    environment: Vec<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_kimi_session_id(
            &project_path,
            &exclusion,
            Some(launch_time_ms),
            &environment,
        )
        .map_err(|e| tracing::debug!(target: "session.capture", "Kimi poll capture failed: {}", e))
        .ok()
        .and_then(validated_session_id)
    }
}

/// Slack (ms) applied to the launch-time floor, mirroring
/// [`KIMI_MTIME_FLOOR_SLACK_MS`]: session files can carry second-granularity
/// mtimes that land below the millisecond launch timestamp and must still
/// count as "touched after launch".
const PRIME_AGENT_MTIME_FLOOR_SLACK_MS: f64 = 2000.0;

/// Byte cap on the first-line header read, mirroring
/// [`PI_HEADER_SCAN_BYTES`]: `BufRead::read_line` otherwise allocates
/// without bound for one hostile or corrupt line. A header longer than this
/// fails to parse and the file is skipped until the next poll.
const PRIME_AGENT_HEADER_SCAN_BYTES: u64 = 64 * 1024;

/// One Prime Agent session, parsed from the first line of a
/// `~/.prime/agent/sessions/<uuid>.jsonl` file. The header carries both the
/// resume id and the working directory; the file name is a different uuid,
/// so the id must come from the header, never from the path.
struct PrimeAgentSession {
    id: String,
    cwd: String,
    mtime_ms: u64,
}

/// Scan `<prime-agent home>/sessions/*.jsonl` and parse each file's first
/// line as a session header. Unreadable files, non-JSON first lines, headers
/// whose `type` is not `session`, and headers missing `id`/`cwd` are skipped:
/// a read-only poll races writers and must tolerate partial files.
fn scan_prime_agent_sessions(sessions_dir: &Path) -> Vec<PrimeAgentSession> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(entries) = resilient_read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl")) {
        // Guarded open, mirroring extract_pi_header_fields: O_NONBLOCK keeps
        // a misnamed FIFO from blocking the poll on open, O_NOFOLLOW refuses
        // symlinked entries, and only regular files are scanned.
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(not(unix))]
        if std::fs::symlink_metadata(entry.path())
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(file) = options.open(entry.path()) else {
            continue;
        };
        if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        let mut first_line = String::new();
        let mut reader = std::io::BufReader::new(file);
        if (&mut reader)
            .take(PRIME_AGENT_HEADER_SCAN_BYTES)
            .read_line(&mut first_line)
            .is_err()
        {
            continue;
        }
        let Ok(header) = serde_json::from_str::<serde_json::Value>(&first_line) else {
            continue;
        };
        if header.get("type").and_then(|v| v.as_str()) != Some("session") {
            continue;
        }
        let (Some(id), Some(cwd)) = (
            header.get("id").and_then(|v| v.as_str()),
            header.get("cwd").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let mtime_ms = std::fs::metadata(entry.path())
            .and_then(|m| m.modified())
            .map(crate::util::system_time_to_ms)
            .unwrap_or(0);
        sessions.push(PrimeAgentSession {
            id: id.to_string(),
            cwd: cwd.to_string(),
            mtime_ms,
        });
    }
    sessions
}

/// Pick the newest unexcluded Prime Agent session whose header `cwd` matches
/// `project_path`. Paths are canonicalized so a symlinked cwd still matches.
/// When `launch_time_ms` is `Some`, only sessions whose file was modified at
/// or after that floor are eligible, so a fresh live poll cannot latch onto a
/// pre-existing conversation before the agent writes the new one. Retroactive
/// recovery passes `None` to allow resuming an older session.
fn select_prime_agent_session(
    sessions: Vec<PrimeAgentSession>,
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let canonical_match = canonicalize_or_raw(project_path);
    let mut candidates: Vec<(String, u64)> = sessions
        .into_iter()
        .filter(|s| !exclusion.contains(&s.id))
        .filter(|s| canonicalize_or_raw(&s.cwd) == canonical_match)
        .map(|s| (s.id, s.mtime_ms))
        .collect();
    if let Some(threshold) = launch_time_ms {
        candidates.retain(|(_, mtime_ms)| {
            (*mtime_ms as f64) + PRIME_AGENT_MTIME_FLOOR_SLACK_MS >= threshold
        });
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
    candidates
        .into_iter()
        .next()
        .map(|(id, _)| id)
        .ok_or_else(|| anyhow::anyhow!("No Prime Agent session found matching project path"))
}

/// Resolve Prime Agent's session directory with the CLI's own env
/// precedence: `PRIME_AGENT_SESSION_DIR`, then its legacy alias
/// `PRIME_AGENT_CODING_AGENT_SESSION_DIR`, then `<coding agent
/// home>/sessions`. The `--session-dir` flag and `settings.json.sessionDir`
/// are invisible to this host-side scan (they are tracked separately).
fn prime_agent_sessions_dir(coding_agent_home: &Path) -> PathBuf {
    for var in [
        "PRIME_AGENT_SESSION_DIR",
        "PRIME_AGENT_CODING_AGENT_SESSION_DIR",
    ] {
        if let Ok(dir) = std::env::var(var) {
            return PathBuf::from(dir);
        }
    }
    coding_agent_home.join("sessions")
}

/// Capture a Prime Agent session ID for `project_path`.
///
/// Reads the first line of every JSONL file in Prime Agent's resolved
/// session directory (`PRIME_AGENT_SESSION_DIR`, then the legacy alias,
/// else `<home>/sessions` where the home resolves from
/// `PRIME_AGENT_CODING_AGENT_DIR`, default `~/.prime/agent`) and returns
/// the id of the newest session whose header `cwd` matches `project_path`,
/// skipping any ids in `exclusion`. `launch_time_ms` gates live polling to
/// sessions touched after this run started (`None` for retroactive recovery).
/// Prime Agent resumes the returned id with `prime-agent --resume <id>`.
pub(crate) fn capture_prime_agent_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: Option<f64>,
) -> Result<String> {
    let home = resolve_agent_home(Some("PRIME_AGENT_CODING_AGENT_DIR"), ".prime/agent")?;
    let sessions = scan_prime_agent_sessions(&prime_agent_sessions_dir(&home));
    select_prime_agent_session(sessions, project_path, exclusion, launch_time_ms)
}

/// Polling closure for Prime Agent session tracking, mirroring
/// [`kimi_poll_fn`]. Host-only: the sessions directory is read from the host,
/// so sandboxed sessions have no poller and start fresh on restart.
pub(crate) fn prime_agent_poll_fn(
    project_path: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        capture_prime_agent_session_id(&project_path, &exclusion, Some(launch_time_ms))
            .map_err(|e| {
                tracing::debug!(target: "session.capture", "Prime Agent poll capture failed: {}", e)
            })
            .ok()
            .and_then(validated_session_id)
    }
}

/// Effective Kimi home for one environment list: `KIMI_CODE_HOME` resolved
/// through the same `$VAR` / bare-key grammar launch applies
/// ([`crate::session::environment::resolve_host_environment_value`]), else the
/// ambient default resolution. An empty resolved value counts as unset; `None`
/// when even the default cannot be resolved (the sharing predicate fails
/// closed on `None`).
fn kimi_home_for_environment(environment: &[String]) -> Option<PathBuf> {
    crate::session::environment::resolve_host_environment_value(environment, "KIMI_CODE_HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .or_else(|| resolve_agent_home(Some("KIMI_CODE_HOME"), ".kimi-code").ok())
        .filter(|home| !home.as_os_str().is_empty())
}

/// Whether another persisted host AoE session shares this one's Kimi store: a
/// resolvable own home plus the same canonicalized project path. When true,
/// the newest matching record in the session index cannot be attributed to
/// this pane, so the acquire-time MRU scan behind
/// [`capture_kimi_session_id`] must not run (#3516). The live poller still
/// runs that scan on shared stores, bounded by its launch-time floor and the
/// exclusion sets.
///
/// `own_resolved_environment` is the caller's
/// [`Instance::resolved_host_environment`] and `own_profile_environment` its
/// static profile list; the own side matches peers against either home so a
/// hook that deterministically mints a different `KIMI_CODE_HOME` still
/// counts its profile siblings as sharing. Peers are judged on their static
/// profile list because minted pairs are runtime state that is deliberately
/// not persisted.
///
/// The walk covers every AoE profile because the store is keyed by resolved
/// home plus cwd, not by profile: two profiles resolving to one home share
/// one store. It fails closed: an unreadable profile list, config, or store,
/// or no resolvable own home all report shared, because ownership that cannot
/// be proven must not license an MRU retarget. Current Kimi peers and peers
/// with a parked Kimi conversation count even when stopped, pane-less,
/// archived, or trashed: the former race during recovery and the latter
/// remain restorable owners. Sandboxed peers are skipped because their Kimi
/// stores are container-private.
pub(crate) fn kimi_store_is_shared(
    current_instance_id: &str,
    current_project_path: &str,
    own_resolved_environment: &[String],
    own_profile_environment: &[String],
) -> bool {
    let canonical_current = canonicalize_or_raw(current_project_path);
    let own_homes = [own_resolved_environment, own_profile_environment]
        .iter()
        .filter_map(|env| kimi_home_for_environment(env))
        .map(|home| canonicalize_or_raw(home.to_string_lossy().as_ref()))
        .collect::<Vec<_>>();
    if own_homes.is_empty() {
        return true;
    }
    let Ok(profiles) = crate::session::list_profiles() else {
        return true;
    };
    for peer_profile in profiles {
        // Judge the namespace before paying for the store read: a peer whose
        // resolved home differs cannot share this store however many rows its
        // sessions.json holds. A successful resolve idempotently reinstalls
        // that profile's status rules; a failed resolve returns shared without
        // installing fallback rules or clearing the prior registry state.
        let Ok(peer_config) = super::config::profile_config::resolve_config(&peer_profile) else {
            return true;
        };
        let Some(peer_home) = kimi_home_for_environment(&peer_config.environment) else {
            return true;
        };
        let peer_home = canonicalize_or_raw(peer_home.to_string_lossy().as_ref());
        if !own_homes.contains(&peer_home) {
            continue;
        }
        let Ok(storage) = crate::session::storage::Storage::new_unwatched(&peer_profile) else {
            return true;
        };
        let Ok(instances) = storage.load() else {
            return true;
        };
        for inst in instances {
            if inst.id == current_instance_id || inst.is_sandboxed() {
                continue;
            }
            // Matched on the built-in the peer resolves to, like the peer
            // filter in `compose_exclusion_with_persisted_peers`: a wrapper
            // captures into the same store, so reading `tool` raw reports a
            // sole owner and licenses the MRU retarget this guard exists to
            // refuse (#3516).
            let owns_kimi = inst.capture_agent_name().unwrap_or(inst.tool.as_str()) == "kimi"
                || inst.prior_tool_session_ids.iter().any(|(tool, prior)| {
                    crate::session::instance::resolved_agent_for(&inst.source_profile, tool, "")
                        .map_or(tool.as_str(), |a| a.name)
                        == "kimi"
                        && prior
                            .agent_session_id
                            .as_deref()
                            .is_some_and(|sid| !sid.is_empty())
                });
            if !owns_kimi {
                continue;
            }
            if canonicalize_or_raw(&inst.project_path) == canonical_current {
                return true;
            }
        }
    }
    false
}

// ─── Hermes session capture ───────────────────────────────────────────────────

const HERMES_COMMAND_TIMEOUT_SECS: u64 = 5;

/// Python one-liner executed via `docker exec` to dump active Hermes sessions.
/// Respects `$HERMES_HOME` env var, falling back to `~/.hermes/state.db`.
///
/// Prints a mode line (`SIGNAL` when the schema has at least one of the
/// `cwd`/`git_repo_root` columns, else `LEGACY`) followed by one
/// TAB-separated `id\tcwd\tgit_repo_root` tuple per active CLI session,
/// newest first. A missing `state.db` exits without output (a read-only poll
/// must never create the store it probes); the poll then fails closed. The
/// script deliberately performs no matching: the recorded
/// cwd values may need host-side canonicalization, so selection happens in
/// Rust (see `select_hermes_session_in_container`).
const HERMES_CONTAINER_CAPTURE_SCRIPT: &str = "import sqlite3, os, sys; \
db=os.path.join(os.environ.get('HERMES_HOME', os.path.expanduser('~/.hermes')), 'state.db'); \
sys.exit(0) if not os.path.exists(db) else None; \
conn=sqlite3.connect(db, timeout=1.0); \
cols={r[1] for r in conn.execute('PRAGMA table_info(sessions)')}; \
has_cwd='cwd' in cols; \
has_root='git_repo_root' in cols; \
print('SIGNAL' if (has_cwd or has_root) else 'LEGACY'); \
cwd_col='cwd' if has_cwd else 'NULL'; \
root_col='git_repo_root' if has_root else 'NULL'; \
[print('%s\\t%s\\t%s' % (r[0], r[1] or '', r[2] or '')) for r in conn.execute('SELECT id, ' + cwd_col + ', ' + root_col + \" FROM sessions WHERE source='cli' AND ended_at IS NULL ORDER BY started_at DESC, id DESC\")]";

/// One active Hermes CLI session row with its recorded project signal.
///
/// `cwd`/`git_repo_root` are `None` when the column is missing from the
/// schema, the value is NULL, or it is empty: such rows carry no usable
/// project signal.
struct HermesSessionRow {
    id: String,
    cwd: Option<String>,
    git_repo_root: Option<String>,
}

/// Snapshot of the active Hermes CLI sessions read from `state.db`.
///
/// `rows` is ordered newest-first (by `started_at`, then `id`).
/// `signal_columns_present` is true when the schema has at least one of the
/// `cwd`/`git_repo_root` columns; selection differs between a signal-capable
/// schema and a legacy one (see `select_hermes_session_id`).
struct HermesSessionScan {
    rows: Vec<HermesSessionRow>,
    signal_columns_present: bool,
}

/// Normalize a Hermes project-signal value: NULL or empty means "no signal".
fn normalize_hermes_signal(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Parse `docker exec` output from [`HERMES_CONTAINER_CAPTURE_SCRIPT`] and
/// pick the conversation for `container_cwd`.
///
/// The mode line is required: anything else means the script drifted from
/// this parser's contract and the poll fails closed. Rows with fewer than
/// three fields are skipped (a cwd containing a newline fragments into such
/// rows; pathological, accepted). Fields are not trimmed, and an empty
/// signal maps to `None`; selection then runs through
/// [`select_hermes_session_id`].
///
/// Residuals, all pathological and mostly benign (they degrade to a fresh
/// start): a newline in the trailing `git_repo_root` field yields a row with
/// a truncated root (its cwd arm still matches); a TAB in a cwd truncates it
/// at the first TAB, so a needle equal to the truncated prefix matches a row
/// whose real cwd differs (a wrong-match corner, accepted for a literal TAB
/// in a path); and row/needle paths are canonicalized against the host
/// filesystem, so a container path that happens to exist on the host as a
/// symlink can compare differently than the value Hermes recorded inside the
/// container.
fn select_hermes_session_in_container(
    output: &[u8],
    container_cwd: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let text = String::from_utf8_lossy(output);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let mode = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("No active Hermes session found"))?;
    let signal_columns_present = match mode {
        "SIGNAL" => true,
        "LEGACY" => false,
        _ => anyhow::bail!("Unexpected Hermes capture output: {mode:?}"),
    };

    let mut rows = Vec::new();
    for line in lines {
        let mut fields = line.splitn(3, '\t');
        let id = fields.next().unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let (cwd, root) = match (fields.next(), fields.next()) {
            (Some(cwd), Some(root)) => (
                normalize_hermes_signal(Some(cwd.to_string())),
                normalize_hermes_signal(Some(root.to_string())),
            ),
            _ => continue,
        };
        rows.push(HermesSessionRow {
            id: id.to_string(),
            cwd,
            git_repo_root: root,
        });
    }

    let scan = HermesSessionScan {
        rows,
        signal_columns_present,
    };
    select_hermes_session_id(&scan, container_cwd, exclusion)
}

/// Pick the Hermes conversation this AoE session should resume.
///
/// With a signal-capable schema (at least one of `cwd`/`git_repo_root`
/// present), only rows whose canonicalized `cwd` or `git_repo_root` equals
/// the canonicalized project path are eligible. The `cwd` signal is tried
/// first across the whole active set and only then `git_repo_root`, because
/// a repo-root match is weaker: it also holds for a conversation started in
/// a subdirectory of the same repo, which may be a sibling AoE session's.
/// Within each pass the most recent row not in `exclusion` wins. Rows with
/// no signal, or with a signal pointing at a different project, are never
/// returned: resuming them would bind the wrong conversation, the #3373 bug
/// class.
///
/// A project path spelled through a now-deleted symlink falls back to its
/// raw spelling in [`canonicalize_or_raw`] and never equals Hermes' recorded
/// physical path, so such sessions start fresh (benign direction, pre-#2858
/// corner shared with the other agents' captures).
///
/// On a legacy schema (neither column present) no row carries a project
/// signal. The sole unclaimed active conversation is returned (unambiguous);
/// with more than one, capture fails closed so the agent starts fresh rather
/// than silently guessing.
///
/// Deliberate divergences from `hermes -c` (which is workspace-scoped via
/// its git-root-or-cwd key and only then falls back to the global
/// most-recent conversation; on a pre-cwd schema Hermes auto-migrates the
/// missing columns on open, its workspace search then finds no
/// signal-bearing rows, and it falls back to the global MRU): AoE requires
/// exact canonicalized equality, considers only active rows (`ended_at IS
/// NULL`, so a cleanly-exited conversation starts fresh by design), orders
/// by `started_at` rather than Hermes' `last_active` recency, and never
/// dips into a global-MRU fallback. That fallback is the mis-attribution
/// bug shape for a project-scoped AoE session.
fn select_hermes_session_id(
    scan: &HermesSessionScan,
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    if scan.signal_columns_present {
        let needle = canonicalize_or_raw(project_path);
        // Two passes, cwd first: a row whose `cwd` IS the project directory is
        // unambiguously this project's conversation, while a `git_repo_root`
        // match only proves same-repo membership and can point at a sibling
        // AoE session running in a subdirectory of the same repo. Scanning
        // both signals in one pass let a newer subdir row outrank this
        // project's own conversation.
        let matched =
            |signal: Option<&str>| signal.is_some_and(|s| canonicalize_or_raw(s) == needle);
        for row in &scan.rows {
            if !exclusion.contains(&row.id) && matched(row.cwd.as_deref()) {
                return Ok(row.id.clone());
            }
        }
        for row in &scan.rows {
            if !exclusion.contains(&row.id) && matched(row.git_repo_root.as_deref()) {
                return Ok(row.id.clone());
            }
        }
        anyhow::bail!("No active Hermes session found matching project path")
    } else {
        let mut unclaimed = scan
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .filter(|id| !exclusion.contains(*id));
        match (unclaimed.next(), unclaimed.next()) {
            (None, _) => anyhow::bail!("No active Hermes session found"),
            (Some(id), None) => Ok(id.to_string()),
            _ => anyhow::bail!(
                "Multiple active Hermes sessions without a project signal; starting fresh"
            ),
        }
    }
}

/// Capture session ID from Hermes's SQLite state database.
///
/// Queries `~/.hermes/state.db` (or `$HERMES_HOME/state.db`) for active CLI
/// sessions. Hermes records the working directory on each local CLI session
/// row (`git_repo_root` only when a gateway or TUI frontend later enriches
/// it), so capture scopes the active set to the canonicalized project path
/// (exact `cwd` or `git_repo_root` match), mirroring how the other agents'
/// captures validate cwd or project hash. On legacy databases without those
/// columns capture fails closed when more than one unclaimed active
/// conversation exists (the poller then yields `None`), so the agent starts
/// fresh instead of silently resuming the wrong conversation. Cross-instance
/// isolation for same-project peers still relies on the exclusion set.
pub(crate) fn capture_hermes_session_id(
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    let hermes_home = resolve_agent_home(Some("HERMES_HOME"), ".hermes")?;
    let db_path = hermes_home.join("state.db");

    let scan = read_hermes_sessions_from_sqlite(&db_path)?;
    select_hermes_session_id(&scan, project_path, exclusion)
}

/// Read active CLI session rows from Hermes's SQLite state database.
///
/// Returns the full active CLI set, newest first, with each row's `cwd` and
/// `git_repo_root` when the schema has those columns (NULL literal
/// otherwise). An `Err` means the DB is unreadable (missing, locked, schema
/// mismatch); the poller will retry on the next tick.
fn read_hermes_sessions_from_sqlite(db_path: &Path) -> Result<HermesSessionScan> {
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

    // Probe the schema per column: hermes adds cwd/git_repo_root in a later
    // schema generation, and older databases lack them. The SELECT arms are
    // built from a fixed whitelist so a partially-migrated schema (one column
    // present) still carries its usable signal instead of failing prepare.
    let (has_cwd, has_git_repo_root) = {
        // PRAGMA table_info on a missing table returns zero rows (no error),
        // so a prepare failure here is a genuinely unreadable store; a missing
        // table surfaces at the SELECT prepare below.
        let mut stmt = conn
            .prepare("PRAGMA table_info(sessions)")
            .context("Failed to prepare Hermes sessions table probe")?;
        let cols = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .context("Failed to read Hermes sessions table columns")?;
        let mut has_cwd = false;
        let mut has_git_repo_root = false;
        for col in cols {
            let col = col.context("Failed to read Hermes session column name")?;
            has_cwd |= col == "cwd";
            has_git_repo_root |= col == "git_repo_root";
        }
        (has_cwd, has_git_repo_root)
    };

    let cwd_expr = if has_cwd { "cwd" } else { "NULL" };
    let root_expr = if has_git_repo_root {
        "git_repo_root"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, {cwd_expr}, {root_expr} FROM sessions \
         WHERE source='cli' AND ended_at IS NULL \
         ORDER BY started_at DESC, id DESC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("Hermes sessions table missing or schema mismatch")?;

    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let cwd: Option<String> = row.get(1)?;
            let root: Option<String> = row.get(2)?;
            Ok(HermesSessionRow {
                id,
                cwd: normalize_hermes_signal(cwd),
                git_repo_root: normalize_hermes_signal(root),
            })
        })
        .context("Failed to query Hermes sessions table")?;

    let mut out: Vec<HermesSessionRow> = Vec::new();
    for row in rows {
        out.push(row.context("Failed to read Hermes session row")?);
    }

    Ok(HermesSessionScan {
        rows: out,
        signal_columns_present: has_cwd || has_git_repo_root,
    })
}

/// Capture a Hermes session ID from inside a Docker container.
///
/// Uses `python3` (guaranteed by Hermes's Python runtime) rather than the
/// `sqlite3` CLI binary, which may not be installed in minimal containers.
pub(crate) fn try_capture_hermes_session_id_in_container(
    container_name: &str,
    container_cwd: &str,
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

    select_hermes_session_in_container(&stdout_bytes, container_cwd, exclusion)
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
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;

    /// Well before the 5-minute live-capture window, in the same absolute-epoch
    /// form the other mtime-ordering tests in this module pin.
    const STALE_JSONL_MTIME: u64 = 1_700_000_000;

    /// Scaffold for the `.claude.json` fallback arm: a project dir whose only
    /// jsonl is older than the 5-minute live-capture window, so
    /// `capture_claude_session_id` falls past the dir scan, plus a
    /// `.claude.json` naming `last_session_id` for that directory. Returns the
    /// encoded transcript dir.
    fn claude_json_fallback_home(
        temp: &tempfile::TempDir,
        project_path: &str,
        stale_transcript: &str,
        last_session_id: &str,
    ) -> std::path::PathBuf {
        let dir = temp
            .path()
            .join(".claude")
            .join("projects")
            .join(encode_claude_project_path(
                &canonicalize_or_raw(project_path).to_string_lossy(),
            ));
        std::fs::create_dir_all(&dir).unwrap();

        let jsonl = dir.join(format!("{stale_transcript}.jsonl"));
        std::fs::write(&jsonl, "").unwrap();
        set_mtime_secs(&jsonl, STALE_JSONL_MTIME);

        // Placed and keyed the way production reads it. `.claude.json` sits
        // *inside* the config dir when `CLAUDE_CONFIG_DIR` selects it (#3410),
        // which this fixture sets, and the `projects` key is canonicalized: on
        // macOS `/tmp` is a symlink, so a raw key would miss and the reject
        // case below would pass without ever reaching the gate.
        std::fs::write(
            temp.path().join(".claude").join(".claude.json"),
            serde_json::json!({
                "projects": {
                    canonicalize_or_raw(project_path).to_string_lossy().to_string():
                        { "lastSessionId": last_session_id }
                }
            })
            .to_string(),
        )
        .unwrap();
        dir
    }

    fn claude_json_env(temp: &tempfile::TempDir) -> EnvGuard {
        EnvGuard::set(&[
            ("HOME", temp.path().to_path_buf()),
            ("CLAUDE_CONFIG_DIR", temp.path().join(".claude")),
        ])
    }

    /// `.claude.json`'s `lastSessionId` is one slot per *directory*, and the
    /// freshness gate around it reads the mtime of `.claude.json` itself,
    /// which any live Claude rewrites, so a months-old value still passes.
    /// Handing that id out for `--resume` when no transcript backs it is a
    /// guaranteed "No conversation found" and a dead pane on every restart.
    #[test]
    #[serial]
    fn claude_json_fallback_rejects_sid_with_no_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = "/tmp/aoe-test-claude-json-phantom";
        let _guard = claude_json_env(&temp);
        claude_json_fallback_home(
            &temp,
            project_path,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "11111111-2222-3333-4444-555555555555",
        );

        let err = capture_claude_session_id(project_path, None, &HashSet::new(), &[])
            .expect_err("a lastSessionId with no transcript must not be captured");
        assert!(
            err.to_string().contains("No active Claude session found"),
            "unexpected error: {err}"
        );
    }

    /// Companion: the arm still works when the named conversation exists. Only
    /// the phantom case is rejected, so a genuinely idle session in a
    /// single-session directory is still recoverable through this path.
    #[test]
    #[serial]
    fn claude_json_fallback_accepts_sid_with_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = "/tmp/aoe-test-claude-json-real";
        let named = "11111111-2222-3333-4444-555555555555";
        let _guard = claude_json_env(&temp);
        let dir = claude_json_fallback_home(
            &temp,
            project_path,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            named,
        );
        // Same stale mtime as the decoy, so the dir scan still falls through
        // and this is reached via the `.claude.json` arm, not the scan.
        let jsonl = dir.join(format!("{named}.jsonl"));
        std::fs::write(&jsonl, "").unwrap();
        set_mtime_secs(&jsonl, STALE_JSONL_MTIME);

        assert_eq!(
            capture_claude_session_id(project_path, None, &HashSet::new(), &[]).unwrap(),
            named
        );
    }

    /// Pin a modification time so mtime ordering in tests never depends on the
    /// host filesystem's timestamp resolution. Opened read-only, which lets the
    /// same call set the mtime of both files and directories on Unix.
    fn set_mtime_secs(path: &Path, secs: u64) {
        std::fs::File::options()
            .read(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            ))
            .unwrap();
    }

    #[test]
    fn canonicalize_or_raw_normalizes_deleted_dirs_lexically() {
        // A stopped worktree session's directory is often deleted while its
        // unnormalized pre-#2858 `project_path` spelling lives on in
        // `sessions.json`. With no filesystem entry to canonicalize, the two
        // spellings must still compare equal via the lexical fallback.
        assert_eq!(
            canonicalize_or_raw("/nonexistent-aoe-test/decoy/../wt"),
            canonicalize_or_raw("/nonexistent-aoe-test/wt"),
        );
        // An existing directory keeps full canonicalization (symlink-aware).
        let temp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(temp.path()).unwrap();
        let spelled = temp
            .path()
            .join("x")
            .join("..")
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(temp.path().join("x")).unwrap();
        assert_eq!(canonicalize_or_raw(&spelled), real);
    }

    #[test]
    fn test_generate_session_uuid() {
        let id = generate_session_uuid();

        // Should be a valid UUID format
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_generate_session_uuid_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_session_uuid()).collect();
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

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new(), &[]);
        assert_eq!(result.unwrap(), uuid_new);

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn test_claude_host_transcript_confirmed_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        let present = "11111111-2222-3333-4444-555555555555";
        let missing = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let file = project_dir.join(format!("{present}.jsonl"));
        std::fs::write(&file, "data\n").unwrap();
        // Existence-only: an old mtime (past the live-capture window) must not
        // read as absent, or an idle real conversation would lose its resume.
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(hour_ago))
            .unwrap();

        let old_val = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());

        assert!(
            !claude_host_transcript_confirmed_absent("/tmp/myproject", present, &[]),
            "a transcript on disk (even stale) must not be reported absent"
        );
        assert!(
            claude_host_transcript_confirmed_absent("/tmp/myproject", missing, &[]),
            "an unwritten sid must be reported confirmed-absent"
        );
        // A project dir that was never created is also confirmed-absent.
        assert!(claude_host_transcript_confirmed_absent(
            "/tmp/never-opened-project",
            present,
            &[]
        ));

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    /// #3399: both Claude reads resolve the config dir the launched pane will
    /// see, so two profiles sharing a cwd each see only their own conversation.
    /// Against the default tree neither transcript exists, so the probe would
    /// call every real conversation absent and downgrade it to `--session-id`,
    /// and the project-dir scan would hand back the other profile's sid.
    #[test]
    #[serial]
    fn claude_reads_resolve_the_profile_scoped_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project = "/tmp/aoe-3399-shared-cwd";
        let personal_sid = "11111111-1111-4111-8111-111111111111";
        let work_sid = "22222222-2222-4222-8222-222222222222";
        let mut homes = Vec::new();
        for (name, sid) in [("personal", personal_sid), ("work", work_sid)] {
            let home = tmp.path().join(format!("claude-{name}"));
            let dir = home
                .join("projects")
                .join(encode_claude_project_path(project));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{sid}.jsonl")), "data\n").unwrap();
            homes.push(vec![format!("CLAUDE_CONFIG_DIR={}", home.display())]);
        }
        let (personal_env, work_env) = (&homes[0], &homes[1]);

        // The process-level dir stands in for "no profile override", and holds
        // neither conversation.
        let _guard = EnvGuard::set(&[("CLAUDE_CONFIG_DIR", tmp.path().join("claude-default"))]);

        for (env, own, other) in [
            (personal_env, personal_sid, work_sid),
            (work_env, work_sid, personal_sid),
        ] {
            assert!(
                !claude_host_transcript_confirmed_absent(project, own, env),
                "{own} lives in this profile's config dir and must read as present"
            );
            assert!(
                claude_host_transcript_confirmed_absent(project, other, env),
                "{other} belongs to the other profile and must not read as present"
            );
            assert_eq!(
                capture_claude_session_id(project, None, &HashSet::new(), env).unwrap(),
                own,
                "the project-dir scan must only see this profile's conversations"
            );
        }

        // No profile override falls back to AoE's own env, which sees neither.
        assert!(claude_host_transcript_confirmed_absent(
            project,
            personal_sid,
            &[]
        ));
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
            capture_claude_session_id("/tmp/myproject", None, &HashSet::new(), &[]).unwrap(),
            uuid_b
        );

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_a), &HashSet::new(), &[])
                .unwrap(),
            uuid_b
        );

        let exclusion: HashSet<String> = std::iter::once(uuid_b.to_string()).collect();
        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_a), &exclusion, &[]).unwrap(),
            uuid_a
        );

        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(uuid_b), &HashSet::new(), &[])
                .unwrap(),
            uuid_b
        );

        let absent = "99999999-9999-9999-9999-999999999999";
        assert_eq!(
            capture_claude_session_id("/tmp/myproject", Some(absent), &HashSet::new(), &[])
                .unwrap(),
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
            capture_claude_session_id("/tmp/myproject", Some(uuid_known), &HashSet::new(), &[])
                .unwrap(),
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
            Vec::new(),
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

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new(), &[]);
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

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new(), &[]);
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

        let result = capture_claude_session_id("/tmp/myproject", None, &HashSet::new(), &[]);
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

    /// A wrapper binary exports its own `CLAUDE_CONFIG_DIR` inside the pane,
    /// after AoE has already decided what to inject at `docker run`, so the
    /// scan used to read a directory the agent never writes and the session
    /// never captured an id (#3638). The live process environment is the only
    /// side of that boundary that answers.
    ///
    /// Linux-only: the discovery reads `/proc`, which is what the container
    /// always is. Skipped elsewhere rather than asserted away.
    #[cfg(target_os = "linux")]
    #[test]
    fn container_snippet_prefers_a_running_process_config_dir() {
        let project_path = "/tmp/aoe-wrapper-scan";
        let injected = tempfile::tempdir().unwrap();
        let wrapper_root = tempfile::tempdir().unwrap();
        // A space in the declared dir is the realistic shape ("Claude
        // Personal"), and the listing has to survive word splitting to find
        // anything under it.
        let wrapper = wrapper_root.path().join("Claude Personal");
        let wrapper_sid = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let injected_sid = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
        for (home, sid) in [
            (injected.path().to_path_buf(), injected_sid),
            (wrapper.clone(), wrapper_sid),
        ] {
            let dir = home
                .join("projects")
                .join(encode_claude_project_path(project_path));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{sid}.jsonl")), "{}\n").unwrap();
        }
        let snippet = claude_container_list_snippet(&encode_claude_project_path(project_path));

        // Nothing running declares another directory: the injected value.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&snippet)
            .env("CLAUDE_CONFIG_DIR", injected.path())
            .output()
            .expect("snippet invocation failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.lines().any(|l| l.trim() == injected_sid),
            "expected the injected dir's transcript, got {stdout:?}"
        );

        // A live process declaring its own outranks it.
        let mut pane = std::process::Command::new("sleep")
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &wrapper)
            .spawn()
            .expect("spawn stand-in for the wrapped agent");
        // `spawn` returning does not guarantee the child has reached `execve`,
        // and until it does its `environ` is still this process's. Wait for the
        // value the scan is meant to find rather than race it.
        let environ = format!("/proc/{}/environ", pane.id());
        let declared = format!("CLAUDE_CONFIG_DIR={}", wrapper.display());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !std::fs::read(&environ)
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .any(|entry| entry == declared.as_bytes())
            })
            .unwrap_or(false)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "stand-in never published its own CLAUDE_CONFIG_DIR"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&snippet)
            .env("CLAUDE_CONFIG_DIR", injected.path())
            .output()
            .expect("snippet invocation failed");
        let _ = pane.kill();
        let _ = pane.wait();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.lines().map(str::trim).collect::<Vec<_>>(),
            vec![wrapper_sid],
            "the wrapper's own directory must win outright"
        );
    }

    /// Runs the production snippet under `sh` against a real directory, so the
    /// guard is exercised without Docker. `CLAUDE_CONFIG_DIR` goes to the child
    /// only, so this needs no `EnvGuard` and no serial key.
    #[cfg(unix)]
    #[test]
    fn test_container_list_snippet_lists_only_regular_files() {
        let sid = "11111111-1111-4111-8111-111111111111";
        // `listed` is what the snippet should emit for each shape. The glob
        // does match a directory, but `ls -tL` on one lists its contents
        // rather than itself, so that entry never reaches `[ -f ]`; the row
        // pins the listing step, not the guard.
        let cases = [
            ("regular", true),
            ("symlink-to-transcript", true),
            ("directory", false),
            ("dangling-link", false),
            ("symlink-cycle", false),
            ("fifo", false),
        ];
        for (kind, listed) in cases {
            let home = tempfile::tempdir().unwrap();
            let project_path = format!("/tmp/container-scan-probe-{kind}");
            let dir = home
                .path()
                .join("projects")
                .join(encode_claude_project_path(&project_path));
            std::fs::create_dir_all(&dir).unwrap();
            let entry = dir.join(format!("{sid}.jsonl"));
            match kind {
                "regular" => std::fs::write(&entry, "{}\n").unwrap(),
                "symlink-to-transcript" => {
                    let target = dir.join("target.data");
                    std::fs::write(&target, "{}\n").unwrap();
                    std::os::unix::fs::symlink(&target, &entry).unwrap();
                    // Age the link past the five-minute gate while its target
                    // stays fresh. Without this the row passes on a link too
                    // young to distinguish lstat from stat, which is the case
                    // that actually breaks. BSD `touch` rejects this date
                    // form, so skip the row where it is unavailable rather
                    // than let it pass without testing anything.
                    let aged = std::process::Command::new("touch")
                        .args(["-h", "-d", "10 minutes ago"])
                        .arg(&entry)
                        .status()
                        .is_ok_and(|s| s.success());
                    if !aged {
                        continue;
                    }
                }
                "directory" => std::fs::create_dir(&entry).unwrap(),
                "dangling-link" => {
                    std::os::unix::fs::symlink(dir.join("gone.jsonl"), &entry).unwrap()
                }
                "symlink-cycle" => std::os::unix::fs::symlink(&entry, &entry).unwrap(),
                "fifo" => {
                    // Skipped rather than failed where `mkfifo` is unavailable;
                    // the other rows still gate the guard.
                    match std::process::Command::new("mkfifo").arg(&entry).status() {
                        Ok(s) if s.success() => {}
                        _ => continue,
                    }
                }
                _ => continue,
            }

            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(claude_container_list_snippet(&encode_claude_project_path(
                    &project_path,
                )))
                .env("CLAUDE_CONFIG_DIR", home.path())
                .output()
                .expect("snippet invocation failed");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                stdout.lines().any(|l| l.trim() == sid),
                listed,
                "{kind}: unexpected listing, stdout {stdout:?}",
            );
        }
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

        let _guard = EnvGuard::set(&[("VIBE_HOME", tmp.path())]);

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

        let _guard = EnvGuard::set(&[("VIBE_HOME", tmp.path())]);

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

        let _guard = EnvGuard::set(&[("VIBE_HOME", tmp.path())]);

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
        let result = build_exclusion_set(
            "nonexistent-instance-id-12345",
            &crate::tmux::LiveSessionSnapshot::new(),
        );
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
    fn test_parse_codex_cwd_validates_declared_ids() {
        let root_uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let child_uuid = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let expected_cwd = Some("/home/user/myproject".to_string());
        let cases = [
            (
                "legacy metadata without ids or type",
                r#"{"payload":{"cwd":"/home/user/myproject"}}"#.to_string(),
                root_uuid,
                expected_cwd.clone(),
            ),
            (
                "matching root ids",
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{root_uuid}","session_id":"{root_uuid}","cwd":"/home/user/myproject"}}}}"#
                ),
                root_uuid,
                expected_cwd,
            ),
            (
                "child points session_id at parent",
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{child_uuid}","session_id":"{root_uuid}","cwd":"/home/user/myproject"}}}}"#
                ),
                child_uuid,
                None,
            ),
            (
                "id differs while session_id matches filename",
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{child_uuid}","session_id":"{root_uuid}","cwd":"/home/user/myproject"}}}}"#
                ),
                root_uuid,
                None,
            ),
            (
                "malformed session_id with matching id",
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{root_uuid}","session_id":"corrupt","cwd":"/home/user/myproject"}}}}"#
                ),
                root_uuid,
                None,
            ),
            (
                "missing cwd",
                format!(r#"{{"payload":{{"id":"{root_uuid}"}}}}"#),
                root_uuid,
                None,
            ),
            (
                "invalid json",
                "not json at all".to_string(),
                root_uuid,
                None,
            ),
        ];

        for (name, line, filename_uuid, expected) in cases {
            assert_eq!(
                parse_codex_cwd_from_json(&line, filename_uuid),
                expected,
                "case: {name}"
            );
        }
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

    #[test]
    #[serial]
    fn test_codex_respects_codex_home_env() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let uuid = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let project_dir = tmp.path().join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_content = serde_json::json!({
            "type": "session_meta",
            "payload": {"cwd": project_dir},
        })
        .to_string();
        std::fs::write(
            sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{}.jsonl", uuid)),
            jsonl_content,
        )
        .unwrap();

        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);

        let result = capture_codex_session_id(project_dir.to_str().unwrap(), &HashSet::new());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), uuid);
    }

    #[test]
    #[serial]
    fn test_codex_capture_reads_compressed_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let uuid = "dddddddd-dddd-dddd-dddd-dddddddddddd";
        let project_dir = tmp.path().join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_content = serde_json::json!({
            "type": "session_meta",
            "payload": {"cwd": project_dir},
        })
        .to_string();
        let compressed = zstd::stream::encode_all(jsonl_content.as_bytes(), 0).unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{uuid}.jsonl.zst")),
            compressed,
        )
        .unwrap();

        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);

        let result = capture_codex_session_id(project_dir.to_str().unwrap(), &HashSet::new());
        assert_eq!(result.unwrap(), uuid);
    }

    #[test]
    #[serial]
    fn test_codex_capture_ignores_newer_child_with_parent_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let root_uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let child_uuid = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let project_dir = tmp.path().join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let root_file = sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{root_uuid}.jsonl"));
        let child_file =
            sessions_dir.join(format!("rollout-2025-03-06T10-31-00-{child_uuid}.jsonl"));
        std::fs::write(
            &root_file,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{root_uuid}","session_id":"{root_uuid}","cwd":"{}"}}}}"#,
                project_dir.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &child_file,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{child_uuid}","session_id":"{root_uuid}","cwd":"{}"}}}}"#,
                project_dir.display()
            ),
        )
        .unwrap();

        let old_time = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&root_file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();

        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);

        let result = capture_codex_session_id(project_dir.to_str().unwrap(), &HashSet::new());
        assert_eq!(result.unwrap(), root_uuid);
    }

    #[test]
    #[serial]
    fn test_codex_capture_empty_sessions_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let _guard = EnvGuard::set(&[("CODEX_HOME", tmp.path())]);

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
    fn test_select_codex_session_in_container_ignores_newer_child_with_parent_session_id() {
        let root_uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let child_uuid = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let stdout = format!(
            "\
===CODEX:1700001000:rollout-2025-01-02T00-00-00-{root_uuid}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{root_uuid}\",\"session_id\":\"{root_uuid}\",\"cwd\":\"/workspace\"}}}}
===END===
===CODEX:1700002000:rollout-2025-01-02T00-01-00-{child_uuid}.jsonl===
{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{child_uuid}\",\"session_id\":\"{root_uuid}\",\"cwd\":\"/workspace\"}}}}
===END===
"
        );
        let result =
            select_codex_session_in_container(stdout.as_bytes(), "/workspace", &HashSet::new())
                .unwrap();
        assert_eq!(result, root_uuid);
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

        let _guard = EnvGuard::set(&[("GEMINI_CLI_HOME", tmp.path())]);

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

        let _guard = EnvGuard::set(&[("GEMINI_CLI_HOME", tmp.path())]);

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

        let _guard = EnvGuard::set(&[("GEMINI_CLI_HOME", tmp.path())]);

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

    /// A seed row for [`seed_hermes_db`]: id, source, started_at, cwd,
    /// git_repo_root.
    type HermesSeedRow<'a> = (&'a str, &'a str, f64, Option<&'a str>, Option<&'a str>);

    /// Seed a Hermes `state.db` under `home` (the dir `HERMES_HOME` points
    /// at). `full_schema` selects the current schema (with
    /// `cwd`/`git_repo_root` columns) or the legacy minimal schema.
    fn seed_hermes_db(home: &Path, rows: &[HermesSeedRow], full_schema: bool) {
        let db_path = home.join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        if full_schema {
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);",
            )
            .unwrap();
            for (id, source, started_at, cwd, root) in rows {
                conn.execute(
                    "INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) \
                     VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                    rusqlite::params![id, source, started_at, cwd, root],
                )
                .unwrap();
            }
        } else {
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);",
            )
            .unwrap();
            for (id, source, started_at, _, _) in rows {
                conn.execute(
                    "INSERT INTO sessions (id, source, started_at, ended_at) VALUES (?1, ?2, ?3, NULL)",
                    rusqlite::params![id, source, started_at],
                )
                .unwrap();
            }
        }
        drop(conn);
    }

    #[test]
    fn test_select_hermes_session_in_container_scoped_parsing() {
        let output = b"SIGNAL\n\
20260429_193246_aaa\t/tmp/hermes-a\t\n\
20260429_193246_bbb\t/tmp/hermes-b\t\n";
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    fn test_select_hermes_session_in_container_scoped_with_exclusion() {
        // Both rows carry the needle's cwd, so the exclusion filter is what
        // separates them (mirrors the DB-level second-match test).
        let output = b"SIGNAL\n\
20260429_193246_aaa\t/tmp/hermes-a\t\n\
20260429_193246_bbb\t/tmp/hermes-a\t\n";
        let mut exclusion = HashSet::new();
        exclusion.insert("20260429_193246_aaa".to_string());
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a", &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_bbb");
    }

    #[test]
    fn test_select_hermes_session_in_container_scoped_no_match() {
        // In SIGNAL mode a row whose cwd points at another project is never
        // returned, even when it is the only active conversation.
        let output = b"SIGNAL\n20260429_193246_aaa\t/tmp/other-project\t\n";
        let result = select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_hermes_session_in_container_legacy_single_row() {
        let output = b"LEGACY\n20260429_193246_aaa\t\t\n";
        let result =
            select_hermes_session_in_container(output, "/tmp/anywhere", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    fn test_select_hermes_session_in_container_legacy_multiple_ambiguous() {
        let output = b"LEGACY\n20260429_193246_aaa\t\t\n20260429_193246_bbb\t\t\n";
        let result = select_hermes_session_in_container(output, "/tmp/anywhere", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_hermes_session_in_container_legacy_multiple_exclusion_narrows() {
        let output = b"LEGACY\n20260429_193246_aaa\t\t\n20260429_193246_bbb\t\t\n";
        let mut exclusion = HashSet::new();
        exclusion.insert("20260429_193246_aaa".to_string());
        let result =
            select_hermes_session_in_container(output, "/tmp/anywhere", &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_bbb");
    }

    #[test]
    fn test_select_hermes_session_in_container_empty_output() {
        let result = select_hermes_session_in_container(b"", "/tmp/hermes-a", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_hermes_session_in_container_whitespace_only_lines_skipped() {
        // Whitespace-only lines before the mode line are tolerated; the first
        // non-empty line must still be the mode line.
        let output = b"  \n\nSIGNAL\n20260429_193246_ccc\t/tmp/hermes-c\t\n  \n";
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-c", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_ccc");
    }

    #[test]
    fn test_select_hermes_session_in_container_garbage_mode_line() {
        // Old id-only output (or a drifted script) must fail closed, not
        // misparse into a bogus id.
        let output = b"20260429_193246_aaa\n20260429_193246_bbb\n";
        let result = select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_select_hermes_session_in_container_malformed_rows_skipped() {
        // A cwd containing a newline fragments the row into lines with fewer
        // than three fields; those are skipped without panicking, and the
        // healthy row still wins.
        let output = b"SIGNAL\n\
20260429_193246_good\t/tmp/hermes-a\t\n\
20260429_193246_bad\t/tmp/hermes-b\nwith-newline\t\n";
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_good");
    }

    #[test]
    fn test_select_hermes_session_in_container_newline_in_root_accepted_truncated() {
        // A newline in the trailing git_repo_root field yields a row with a
        // truncated root (documented residual); the cwd arm still matches.
        let output = b"SIGNAL\n\
20260429_193246_root\t/tmp/hermes-a\t/tmp/root\npart2\t\n";
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_root");
    }

    #[test]
    fn test_select_hermes_session_in_container_tab_in_cwd_truncates() {
        // A cwd containing a TAB truncates at the first TAB (documented
        // residual). The byte input mirrors what the script emits for a real
        // cwd "/tmp/hermes-a<TAB>rest" with an empty script-side root field:
        // id, the pre-TAB cwd, the remainder, then the empty root field
        // separator. splitn(3) puts the remainder into the parser-side root
        // ("rest\t"), so a needle equal to the truncated cwd matches; a
        // different needle must not.
        let output = b"SIGNAL\n20260429_193246_aaa\t/tmp/hermes-a\trest\t\n";
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a/sub", &HashSet::new());
        assert!(result.is_err());
        let result =
            select_hermes_session_in_container(output, "/tmp/hermes-a", &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_matches_project_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[(
                "20260429_193246_aaa",
                "cli",
                1000.0,
                Some(&project_str),
                None,
            )],
            true,
        );
        let result = capture_hermes_session_id(&project_str, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_prefers_exact_cwd_over_repo_root() {
        // A conversation started in a subdirectory of the same repo carries
        // this project's git_repo_root once a hermes gateway/TUI enriches it
        // (or backfill_repo_roots runs), and is usually a sibling AoE
        // session's. The row whose cwd IS this project must win even when the
        // subdir row is newer.
        let tmp = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap();
        let repo_str = repo.to_string_lossy().to_string();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_str = std::fs::canonicalize(&sub)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                (
                    "20260429_193246_own",
                    "cli",
                    1000.0,
                    Some(&repo_str),
                    Some(&repo_str),
                ),
                (
                    "20260429_193246_sub",
                    "cli",
                    2000.0,
                    Some(&sub_str),
                    Some(&repo_str),
                ),
            ],
            true,
        );
        assert_eq!(
            capture_hermes_session_id(&repo_str, &HashSet::new()).unwrap(),
            "20260429_193246_own"
        );
    }

    #[test]
    #[serial]
    fn test_capture_hermes_matches_git_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_str = std::fs::canonicalize(&sub)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[(
                "20260429_193246_aaa",
                "cli",
                1000.0,
                Some(&sub_str),
                Some(&project_str),
            )],
            true,
        );
        let result = capture_hermes_session_id(&project_str, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_subdir_project_matches_via_cwd() {
        // A project that is a proper subdir of a git repo records
        // git_repo_root != project; the cwd arm must still match (the empty
        // git_repo_root gate of hermes' own clause would drop this).
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("pkg");
        std::fs::create_dir_all(&project).unwrap();
        let project_str = std::fs::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let repo = std::fs::canonicalize(tmp.path()).unwrap();
        let repo_str = repo.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[(
                "20260429_193246_aaa",
                "cli",
                1000.0,
                Some(&project_str),
                Some(&repo_str),
            )],
            true,
        );
        let result = capture_hermes_session_id(&project_str, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn test_capture_hermes_symlinked_project_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let real_str = std::fs::canonicalize(&real)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let link_str = link.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[("20260429_193246_aaa", "cli", 1000.0, Some(&real_str), None)],
            true,
        );
        let result = capture_hermes_session_id(&link_str, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_unnormalized_project_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let spelled = project
            .join("decoy")
            .join("..")
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[(
                "20260429_193246_aaa",
                "cli",
                1000.0,
                Some(&project_str),
                None,
            )],
            true,
        );
        let result = capture_hermes_session_id(&spelled, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_ignores_newer_foreign_row() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_str = std::fs::canonicalize(&other)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        // The foreign row is newer; the matching row must still win.
        seed_hermes_db(
            tmp.path(),
            &[
                (
                    "20260429_193246_aaa",
                    "cli",
                    1000.0,
                    Some(&project_str),
                    None,
                ),
                ("20260429_193246_bbb", "cli", 2000.0, Some(&other_str), None),
            ],
            true,
        );
        let result = capture_hermes_session_id(&project_str, &HashSet::new()).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_foreign_only_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_str = std::fs::canonicalize(&other)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[("20260429_193246_aaa", "cli", 1000.0, Some(&other_str), None)],
            true,
        );
        // Never resume a conversation attributable to another project.
        assert!(capture_hermes_session_id(&project_str, &HashSet::new()).is_err());
    }

    #[test]
    #[serial]
    fn test_capture_hermes_null_cwd_never_resumed() {
        // Full-schema rows with NULL (or empty) cwd carry no project signal
        // and are never returned; resuming one is the #3373 bug shape. Hermes
        // stamps cwd on the row it creates for a local CLI launch, and records
        // None for a non-local TERMINAL_ENV backend. Two known gaps leave a
        // no-signal row this capture then skips: a `/new` inside a running
        // hermes pane rotates to a fresh row created without a cwd, and a
        // hermes upgrade that predates the column leaves history unstamped.
        // Such a session keeps whatever agent_session_id it already had rather
        // than following the rotation.
        for cwd in [None, Some("")] {
            let tmp = tempfile::tempdir().unwrap();
            let project = std::fs::canonicalize(tmp.path()).unwrap();
            let project_str = project.to_string_lossy().to_string();
            let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
            seed_hermes_db(
                tmp.path(),
                &[("20260429_193246_aaa", "cli", 1000.0, cwd, None)],
                true,
            );
            assert!(
                capture_hermes_session_id(&project_str, &HashSet::new()).is_err(),
                "NULL/empty cwd row must not be resumed (cwd={cwd:?})"
            );
        }
    }

    #[test]
    #[serial]
    fn test_capture_hermes_multiple_null_cwd_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                ("20260429_193246_aaa", "cli", 1000.0, None, None),
                ("20260429_193246_bbb", "cli", 2000.0, None, None),
            ],
            true,
        );
        assert!(capture_hermes_session_id(&project_str, &HashSet::new()).is_err());
    }

    #[test]
    #[serial]
    fn test_capture_hermes_legacy_multiple_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                ("20260429_193246_aaa", "cli", 1000.0, None, None),
                ("20260429_193246_bbb", "cli", 2000.0, None, None),
            ],
            false,
        );
        assert!(capture_hermes_session_id("/tmp/hermes-proj", &HashSet::new()).is_err());
    }

    #[test]
    #[serial]
    fn test_capture_hermes_all_matched_excluded_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                (
                    "20260429_193246_aaa",
                    "cli",
                    1000.0,
                    Some(&project_str),
                    None,
                ),
                (
                    "20260429_193246_bbb",
                    "cli",
                    2000.0,
                    Some(&project_str),
                    None,
                ),
            ],
            true,
        );
        // A same-project peer owns this project's conversations; never dip
        // into no-signal or foreign rows.
        let exclusion = HashSet::from([
            "20260429_193246_aaa".to_string(),
            "20260429_193246_bbb".to_string(),
        ]);
        assert!(capture_hermes_session_id(&project_str, &exclusion).is_err());
    }

    #[test]
    #[serial]
    fn test_capture_hermes_exclusion_picks_second_match() {
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                (
                    "20260429_193246_aaa",
                    "cli",
                    1000.0,
                    Some(&project_str),
                    None,
                ),
                (
                    "20260429_193246_bbb",
                    "cli",
                    2000.0,
                    Some(&project_str),
                    None,
                ),
            ],
            true,
        );
        let mut exclusion = HashSet::new();
        exclusion.insert("20260429_193246_bbb".to_string());
        let result = capture_hermes_session_id(&project_str, &exclusion).unwrap();
        assert_eq!(result, "20260429_193246_aaa");
    }

    #[test]
    #[serial]
    fn test_capture_hermes_cwd_only_and_root_only_schemas() {
        // Partially-migrated schemas must not fail prepare and must use the
        // single present column (F1 regression).
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_str = std::fs::canonicalize(&other)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);

        // cwd-only schema.
        let cwd_db = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(cwd_db.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, cwd TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, ended_at, cwd) VALUES (?1, 'cli', 1000.0, NULL, ?2)",
            rusqlite::params!["20260429_193246_aaa", project_str],
        )
        .unwrap();
        drop(conn);
        let _cwd_guard = EnvGuard::set(&[("HERMES_HOME", cwd_db.path())]);
        assert_eq!(
            capture_hermes_session_id(&project_str, &HashSet::new()).unwrap(),
            "20260429_193246_aaa"
        );
        drop(_cwd_guard);

        // root-only schema: a matching row resolves via the root arm.
        let root_db = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(root_db.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, git_repo_root TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, ended_at, git_repo_root) VALUES (?1, 'cli', 1000.0, NULL, ?2)",
            rusqlite::params!["20260429_193246_bbb", project_str],
        )
        .unwrap();
        drop(conn);
        let _root_guard = EnvGuard::set(&[("HERMES_HOME", root_db.path())]);
        assert_eq!(
            capture_hermes_session_id(&project_str, &HashSet::new()).unwrap(),
            "20260429_193246_bbb"
        );
        // A root-only row pointing at another project must not match.
        let other_db = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(other_db.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, git_repo_root TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, ended_at, git_repo_root) VALUES (?1, 'cli', 1000.0, NULL, ?2)",
            rusqlite::params!["20260429_193246_ccc", other_str],
        )
        .unwrap();
        drop(conn);
        let _other_guard = EnvGuard::set(&[("HERMES_HOME", other_db.path())]);
        assert!(capture_hermes_session_id(&project_str, &HashSet::new()).is_err());
    }

    #[test]
    #[serial]
    fn test_hermes_poll_fn_matches_own_project() {
        // Issue story 1 at the poller level: the closure resolves the
        // project-scoped conversation.
        let tmp = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(tmp.path()).unwrap();
        let project_str = project.to_string_lossy().to_string();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[(
                "20260429_193246_aaa",
                "cli",
                1000.0,
                Some(&project_str),
                None,
            )],
            true,
        );

        let poll = hermes_poll_fn(project_str, "test-instance".to_string(), HashSet::new());
        assert_eq!(poll(), Some("20260429_193246_aaa".to_string()));
    }

    #[test]
    #[serial]
    fn test_hermes_poll_fn_legacy_multi_row_returns_none() {
        // Issue story 2: with multiple active conversations and no project
        // signal, the poller yields None so the agent starts fresh instead of
        // silently resuming a wrong conversation.
        let tmp = tempfile::tempdir().unwrap();
        let _hermes = EnvGuard::set(&[("HERMES_HOME", tmp.path())]);
        seed_hermes_db(
            tmp.path(),
            &[
                ("20260429_193246_aaa", "cli", 1000.0, None, None),
                ("20260429_193246_bbb", "cli", 2000.0, None, None),
            ],
            false,
        );

        let poll = hermes_poll_fn(
            "/tmp/hermes-proj".to_string(),
            "test-instance".to_string(),
            HashSet::new(),
        );
        assert_eq!(poll(), None);
    }

    #[test]
    #[serial]
    fn test_capture_hermes_basic() {
        // Legacy minimal schema: a single active conversation is unambiguous
        // and is returned (sole-row rule).
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
        // Legacy schema, two active rows, one claimed by a peer: the single
        // unclaimed row is unambiguous and returned.
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

    #[test]
    fn test_hermes_container_script_modes() {
        // Run the literal HERMES_CONTAINER_CAPTURE_SCRIPT against a temp db
        // with the host python3 (no docker needed), verifying the SIGNAL and
        // LEGACY mode lines and the TAB row format. No process-global state
        // is touched (HERMES_HOME goes to the child only), so no #[serial].
        // Skips when python3 is unavailable.
        let ok = std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        // Missing store: the read-only poll must not create state.db and must
        // produce no output (the parser then fails closed).
        let empty_home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("python3")
            .arg("-c")
            .arg(HERMES_CONTAINER_CAPTURE_SCRIPT)
            .env("HERMES_HOME", empty_home.path())
            .output()
            .unwrap();
        assert!(out.stdout.is_empty());
        assert!(
            !empty_home.path().join("state.db").exists(),
            "script must not create the store it probes"
        );

        // SIGNAL arm: full schema, two rows in different projects.
        let signal_db = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(signal_db.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);
             INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('20260429_193246_aaa','cli',1000.0,NULL,'/tmp/proj-a',NULL);
             INSERT INTO sessions (id, source, started_at, ended_at, cwd, git_repo_root) VALUES ('20260429_193246_bbb','cli',2000.0,NULL,'/tmp/proj-b',NULL);",
        )
        .unwrap();
        drop(conn);
        let out = std::process::Command::new("python3")
            .arg("-c")
            .arg(HERMES_CONTAINER_CAPTURE_SCRIPT)
            .env("HERMES_HOME", signal_db.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some("SIGNAL"));
        assert_eq!(lines.next(), Some("20260429_193246_bbb\t/tmp/proj-b\t"));
        assert_eq!(lines.next(), Some("20260429_193246_aaa\t/tmp/proj-a\t"));
        assert_eq!(lines.next(), None);

        // LEGACY arm: minimal schema, no signal columns.
        let legacy_db = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(legacy_db.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
             INSERT INTO sessions VALUES ('20260429_193246_aaa','cli',1000.0,NULL);",
        )
        .unwrap();
        drop(conn);
        let out = std::process::Command::new("python3")
            .arg("-c")
            .arg(HERMES_CONTAINER_CAPTURE_SCRIPT)
            .env("HERMES_HOME", legacy_db.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some("LEGACY"));
        assert_eq!(lines.next(), Some("20260429_193246_aaa\t\t"));
        assert_eq!(lines.next(), None);
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
    fn test_kimi_home_rejects_empty_ambient_fallback() {
        let _env = EnvGuard::set(&[("KIMI_CODE_HOME", "")]);
        assert!(
            kimi_home_for_environment(&[]).is_none(),
            "an explicitly empty ambient home must not become a relative store path"
        );
    }

    #[test]
    fn test_read_kimi_session_index_applies_deletions_and_last_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = tmp.path().join("session_index.jsonl");
        // A record, an update to it, a second record, a deletion of the second,
        // a malformed line, and a record missing workDir (skipped).
        std::fs::write(
            &index,
            concat!(
                r#"{"sessionId":"session_a","sessionDir":"/s/a-old","workDir":"/p/one"}"#,
                "\n",
                r#"{"sessionId":"session_a","sessionDir":"/s/a","workDir":"/p/one"}"#,
                "\n",
                r#"{"sessionId":"session_b","sessionDir":"/s/b","workDir":"/p/two"}"#,
                "\n",
                r#"{"sessionId":"session_b","deleted":true}"#,
                "\n",
                "not json at all\n",
                r#"{"sessionId":"session_c","workDir":"/p/three"}"#,
                "\n",
            ),
        )
        .unwrap();

        let sessions = read_kimi_session_index(&index).unwrap();
        let by_id: std::collections::HashMap<&str, &str> = sessions
            .iter()
            .map(|s| (s.id.as_str(), s.session_dir.as_str()))
            .collect();
        // session_a survives with its updated dir; session_b was tombstoned;
        // session_c had no sessionDir; the malformed line was skipped.
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id.get("session_a"), Some(&"/s/a"));
    }

    #[test]
    fn test_read_kimi_session_index_missing_file_errs() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read_kimi_session_index(&tmp.path().join("nope.jsonl")).is_err());
    }

    #[test]
    fn test_select_kimi_session_matches_workdir() {
        let proj = tempfile::TempDir::new().unwrap();
        let proj_path = proj.path().to_str().unwrap().to_string();
        let sessions = vec![
            KimiSession {
                id: "session_match".to_string(),
                session_dir: proj.path().join("sdir").to_string_lossy().into_owned(),
                work_dir: proj_path.clone(),
            },
            KimiSession {
                id: "session_other".to_string(),
                session_dir: "/s/other".to_string(),
                work_dir: "/some/other/project".to_string(),
            },
        ];
        let got = select_kimi_session(&sessions, &proj_path, &HashSet::new(), None).unwrap();
        assert_eq!(got, "session_match");
    }

    #[test]
    fn test_select_kimi_session_launch_floor_excludes_pre_launch_sessions() {
        // A real directory carries an mtime of ~now; a nonexistent one reads as
        // mtime 0. With a launch floor well in the past, only the real (fresh)
        // session is eligible; with a future floor, neither is.
        let proj = tempfile::TempDir::new().unwrap();
        let proj_path = proj.path().to_str().unwrap().to_string();
        let now_ms = crate::util::now_ms() as f64;
        let sessions = vec![
            KimiSession {
                id: "session_fresh".to_string(),
                session_dir: proj.path().join("live").to_string_lossy().into_owned(),
                work_dir: proj_path.clone(),
            },
            KimiSession {
                id: "session_stale".to_string(),
                session_dir: "/does/not/exist".to_string(),
                work_dir: proj_path.clone(),
            },
        ];
        std::fs::create_dir(proj.path().join("live")).unwrap();

        // Floor 10s in the past: the fresh dir passes, the mtime-0 stale one is
        // filtered out.
        assert_eq!(
            select_kimi_session(
                &sessions,
                &proj_path,
                &HashSet::new(),
                Some(now_ms - 10_000.0)
            )
            .unwrap(),
            "session_fresh"
        );
        // Floor far in the future: nothing qualifies.
        assert!(select_kimi_session(
            &sessions,
            &proj_path,
            &HashSet::new(),
            Some(now_ms + 1_000_000.0)
        )
        .is_err());
    }

    #[test]
    fn test_select_kimi_session_skips_excluded_and_errs_on_no_match() {
        let proj = tempfile::TempDir::new().unwrap();
        let proj_path = proj.path().to_str().unwrap().to_string();
        let sessions = vec![
            KimiSession {
                id: "session_excluded".to_string(),
                session_dir: "/s/x".to_string(),
                work_dir: proj_path.clone(),
            },
            KimiSession {
                id: "session_keep".to_string(),
                session_dir: "/s/k".to_string(),
                work_dir: proj_path.clone(),
            },
        ];
        // Excluding the one leaves exactly one candidate, so the result is
        // deterministic regardless of directory mtimes.
        let exclusion: HashSet<String> = ["session_excluded".to_string()].into_iter().collect();
        assert_eq!(
            select_kimi_session(&sessions, &proj_path, &exclusion, None).unwrap(),
            "session_keep"
        );
        // No session matches an unrelated project path.
        assert!(select_kimi_session(&sessions, "/no/such/project", &HashSet::new(), None).is_err());
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
    #[cfg(unix)]
    fn test_opencode_capture_matches_symlinked_project_path_end_to_end() {
        // End-to-end lock for the read-command self-heal path on REAL opencode
        // storage: `try_capture_opencode_session_id` must resolve a session id
        // from a real `opencode.db` even when the caller's project path reaches
        // the same directory through a symlink. opencode records its
        // `directory` as `realpathSync(cwd)` (symlinks resolved), while an aoe
        // session's stored `project_path` may still contain a symlink
        // component (on macOS every /tmp and /var path is one). The match
        // survives only because `filter_agent_sessions` canonicalizes BOTH
        // sides; this test would fail if that symmetry regressed. The
        // fake-codex e2e cannot cover this because it uses a jsonl store, not
        // opencode's SQLite.
        let tmp = tempfile::tempdir().unwrap();
        let real_project = tmp.path().join("real-project");
        std::fs::create_dir(&real_project).unwrap();
        let canonical_project = std::fs::canonicalize(&real_project).unwrap();

        let link = tmp.path().join("link-to-project");
        std::os::unix::fs::symlink(&real_project, &link).unwrap();

        let (_dir, db_path) = create_opencode_test_db(&[(
            "ses_symlink_target",
            canonical_project.to_str().unwrap(),
            5_000,
        )]);

        let old = std::env::var("OPENCODE_DB").ok();
        std::env::set_var("OPENCODE_DB", db_path.to_str().unwrap());

        let result = try_capture_opencode_session_id(link.to_str().unwrap(), &HashSet::new(), None);

        match old {
            Some(v) => std::env::set_var("OPENCODE_DB", v),
            None => std::env::remove_var("OPENCODE_DB"),
        }

        assert_eq!(
            result.ok().as_deref(),
            Some("ses_symlink_target"),
            "capture must match a canonicalized stored directory when the caller \
             path reaches it through a symlink"
        );
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
            Vec::new(),
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
            Vec::new(),
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
            capture_claude_session_id("/tmp/myproject", Some(uuid_anchor), &HashSet::new(), &[])
                .unwrap(),
            uuid_active
        );

        match old_val {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_inner_bounds_drain_when_grandchild_holds_pipe() {
        // The immediate child (sh) exits fast but backgrounds a `sleep` that
        // inherits the stdout pipe, so the write end never closes. The drain
        // must still return by the deadline instead of blocking on read_to_end;
        // `sleep 10` (>> the 4s assertion) makes an unbounded recv visibly fail.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 10 & printf done"]);
        let start = Instant::now();
        let out = run_with_timeout_inner(cmd, Duration::from_millis(500), "grandchild-test", None)
            .expect("the sh child exits quickly, so a buffer is produced");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "drain must be bounded by the deadline even while the pipe stays open"
        );
        assert!(out.is_empty() || out == b"done");
    }

    /// Every entry here has the exact shape a real transcript has, a `.jsonl`
    /// extension and a UUID stem, and nothing behind it. `main` hands all four
    /// back as resume ids: `DirEntry::metadata` is an `lstat`, which succeeds
    /// on all of them and reports the creation time, so they read as *fresh*
    /// and win the `best` comparison outright.
    #[test]
    fn test_scan_skips_entries_that_are_not_regular_files() {
        let sid = "11111111-1111-4111-8111-111111111111";
        for kind in ["directory", "dangling-link", "symlink-cycle", "fifo"] {
            let home = tempfile::tempdir().unwrap();
            let project_path = format!("/tmp/scan-probe-{kind}");
            let project = std::path::Path::new(&project_path);
            let dir = home
                .path()
                .join("projects")
                .join(encode_claude_project_path(&project.to_string_lossy()));
            std::fs::create_dir_all(&dir).unwrap();
            let entry = dir.join(format!("{sid}.jsonl"));
            match kind {
                "directory" => std::fs::create_dir(&entry).unwrap(),
                #[cfg(unix)]
                "dangling-link" => {
                    std::os::unix::fs::symlink(dir.join("gone.jsonl"), &entry).unwrap()
                }
                // `fs::metadata` returns `ELOOP` here rather than spinning, so
                // the `Err` arm is what rejects this one, not `is_file`.
                #[cfg(unix)]
                "symlink-cycle" => std::os::unix::fs::symlink(&entry, &entry).unwrap(),
                #[cfg(unix)]
                "fifo" => {
                    // Skipped rather than failed where `mkfifo` is unavailable;
                    // the other three rows still gate the guard.
                    match std::process::Command::new("mkfifo").arg(&entry).status() {
                        Ok(s) if s.success() => {}
                        _ => continue,
                    }
                }
                _ => continue,
            }

            assert_eq!(
                scan_claude_project_dir(home.path(), project, None, &HashSet::new()).unwrap(),
                None,
                "{kind} must not be handed back as a resume id",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_scan_follows_a_symlinked_transcript() {
        let home = tempfile::tempdir().unwrap();
        let project = std::path::Path::new("/tmp/scan-link-probe");
        let dir = home
            .path()
            .join("projects")
            .join(encode_claude_project_path(&project.to_string_lossy()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = home.path().join("real.jsonl");
        std::fs::write(&real, "{}\n").unwrap();
        let sid = "22222222-2222-4222-8222-222222222222";
        std::os::unix::fs::symlink(&real, dir.join(format!("{sid}.jsonl"))).unwrap();

        let found = scan_claude_project_dir(home.path(), project, None, &HashSet::new()).unwrap();
        assert_eq!(found.map(|(id, _)| id), Some(sid.to_string()));
    }

    /// Write one Prime Agent session file into `dir` and return its path.
    fn write_prime_session(dir: &Path, name: &str, id: &str, cwd: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\
                 \"timestamp\":\"2026-08-23T00:00:00.000Z\",\"cwd\":\"{cwd}\",\"rlmDepth\":0}}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn test_scan_prime_agent_sessions_parses_headers_and_skips_noise() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        write_prime_session(&sessions_dir, "aaa.jsonl", "id-valid", "/tmp/proj");
        // A non-session first line (a mid-file event) must be skipped.
        std::fs::write(
            sessions_dir.join("bbb.jsonl"),
            "{\"type\":\"model_change\",\"id\":\"x\"}\n",
        )
        .unwrap();
        // Malformed JSON, a header without cwd, and a non-jsonl extension are
        // all ignored by the scan.
        std::fs::write(sessions_dir.join("ccc.jsonl"), "not json at all\n").unwrap();
        std::fs::write(
            sessions_dir.join("ddd.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"id-nocwd\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("eee.txt"),
            "{\"type\":\"session\",\"id\":\"id-txt\",\"cwd\":\"/tmp/proj\"}\n",
        )
        .unwrap();
        // A missing directory scans empty rather than erroring.
        assert!(scan_prime_agent_sessions(&tmp.path().join("nope")).is_empty());

        let scanned = scan_prime_agent_sessions(&sessions_dir);
        let mut ids: Vec<&str> = scanned.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["id-valid"]);
    }

    #[test]
    fn test_scan_prime_agent_sessions_skips_oversized_header() {
        // A first line longer than PRIME_AGENT_HEADER_SCAN_BYTES is read
        // truncated, fails JSON parsing, and the file is skipped instead of
        // allocating without bound (mirror of the pi oversized-line pin).
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let mut oversized = String::from(
            "{\"type\":\"session\",\"id\":\"id-big\",\"cwd\":\"/tmp/proj\",\"pad\":\"",
        );
        oversized.push_str(&"x".repeat(96 * 1024));
        oversized.push_str("\"}\n");
        std::fs::write(sessions_dir.join("big.jsonl"), &oversized).unwrap();

        assert!(scan_prime_agent_sessions(&sessions_dir).is_empty());
    }

    /// Unix-only: a FIFO named `*.jsonl` must be skipped without blocking
    /// the scan, and a symlinked entry must not be followed. If this test
    /// ever hangs, the guarded open regressed to plain `File::open`.
    #[test]
    #[cfg(unix)]
    fn test_scan_prime_agent_sessions_skips_fifo_and_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let fifo = sessions_dir.join("fifo.jsonl");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        symlink(
            tmp.path().join("elsewhere.jsonl"),
            sessions_dir.join("link.jsonl"),
        )
        .unwrap();

        assert!(scan_prime_agent_sessions(&sessions_dir).is_empty());
    }

    #[test]
    #[serial]
    fn test_capture_prime_agent_session_id_selects_newest_matching_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let proj_path = tmp.path().join("proj").to_str().unwrap().to_string();
        write_prime_session(&sessions_dir, "old.jsonl", "id-old", &proj_path);
        set_mtime_secs(&sessions_dir.join("old.jsonl"), 1_000);
        write_prime_session(&sessions_dir, "new.jsonl", "id-new", &proj_path);
        set_mtime_secs(&sessions_dir.join("new.jsonl"), 2_000);
        write_prime_session(
            &sessions_dir,
            "other.jsonl",
            "id-other",
            "/some/other/project",
        );
        set_mtime_secs(&sessions_dir.join("other.jsonl"), 3_000);

        let _guard = EnvGuard::set(&[("PRIME_AGENT_CODING_AGENT_DIR", tmp.path())]);
        let got = capture_prime_agent_session_id(&proj_path, &HashSet::new(), None).unwrap();
        assert_eq!(got, "id-new");
        // The exclusion set drops the newest match so the older one wins.
        let excluded: HashSet<String> = ["id-new".to_string()].into_iter().collect();
        let got = capture_prime_agent_session_id(&proj_path, &excluded, None).unwrap();
        assert_eq!(got, "id-old");
        // No session matches a different project.
        assert!(capture_prime_agent_session_id("/elsewhere", &HashSet::new(), None).is_err());
    }

    #[test]
    #[serial]
    fn test_capture_prime_agent_session_id_launch_floor_excludes_stale_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let proj_path = tmp.path().join("proj").to_str().unwrap().to_string();
        write_prime_session(&sessions_dir, "stale.jsonl", "id-stale", &proj_path);
        set_mtime_secs(&sessions_dir.join("stale.jsonl"), 1_000);

        let _guard = EnvGuard::set(&[("PRIME_AGENT_CODING_AGENT_DIR", tmp.path())]);
        // A floor far in the future rejects the stale file; retroactive
        // recovery (None) still finds it.
        assert!(
            capture_prime_agent_session_id(&proj_path, &HashSet::new(), Some(f64::MAX / 2.0))
                .is_err()
        );
        assert_eq!(
            capture_prime_agent_session_id(&proj_path, &HashSet::new(), None).unwrap(),
            "id-stale"
        );
    }

    #[test]
    #[serial]
    fn test_capture_prime_agent_session_id_honors_session_dir_overrides() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The default location holds a decoy that must be ignored whenever an
        // override points elsewhere.
        let default_sessions = tmp.path().join("sessions");
        std::fs::create_dir(&default_sessions).unwrap();
        write_prime_session(
            &default_sessions,
            "default.jsonl",
            "id-default",
            "/tmp/test",
        );

        let proj_path = "/tmp/test".to_string();
        // Primary override wins over the default location.
        let redirected = tmp.path().join("redirected");
        std::fs::create_dir(&redirected).unwrap();
        write_prime_session(&redirected, "seed.jsonl", "id-override", &proj_path);
        let _primary = EnvGuard::set(&[
            ("PRIME_AGENT_CODING_AGENT_DIR", tmp.path()),
            ("PRIME_AGENT_SESSION_DIR", redirected.as_path()),
        ]);
        assert_eq!(
            capture_prime_agent_session_id(&proj_path, &HashSet::new(), None).unwrap(),
            "id-override"
        );
        // An ambient PRIME_AGENT_SESSION_DIR from the developer's own shell
        // must not shadow the legacy alias under test.
        drop(_primary);
        let _unset_primary = EnvGuard::unset(&["PRIME_AGENT_SESSION_DIR"]);

        // Legacy alias applies when the primary override is unset.
        let legacy = tmp.path().join("legacy");
        std::fs::create_dir(&legacy).unwrap();
        write_prime_session(&legacy, "seed.jsonl", "id-legacy", &proj_path);
        let _legacy = EnvGuard::set(&[
            ("PRIME_AGENT_CODING_AGENT_DIR", tmp.path()),
            ("PRIME_AGENT_CODING_AGENT_SESSION_DIR", legacy.as_path()),
        ]);
        assert_eq!(
            capture_prime_agent_session_id(&proj_path, &HashSet::new(), None).unwrap(),
            "id-legacy"
        );
    }
}

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
/// Whether we can affirmatively prove Claude has no persisted transcript for
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
            PiSidecarSource::SandboxDir(ref dir) => dir
                .parent()
                .and_then(Path::parent)
                .filter(|root| root.join("aoe-session").join(&instance_id) == *dir)
                .and_then(|root| crate::session::AnchoredDir::open(root).ok())
                .and_then(|root| {
                    root.read_regular(
                        &Path::new("aoe-session")
                            .join(&instance_id)
                            .join("session_id"),
                        4096,
                    )
                    .ok()
                    .flatten()
                })
                .and_then(|raw| String::from_utf8(raw).ok())
                .map(|raw| raw.trim().to_string())
                .filter(|id| Uuid::parse_str(id).is_ok()),
            PiSidecarSource::HostHooks => crate::hooks::read_hook_session_id(&instance_id),
        };
        id.and_then(validated_session_id)
            .map(crate::session::poller::SessionIdObservation::instance_sidecar)
    }
}

pub(crate) const MAX_SESSION_ID_LEN: usize = 256;

pub(crate) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id.len() <= MAX_SESSION_ID_LEN
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

/// Build the capture exclusion set from live pane ownership, caller-provided
/// exclusions, and conversation ids parked by peer engine swaps.
///
/// A peer still owns every parked id until it swaps back. Exclude all of
/// them rather than re-resolving a raw alias through mutable profile config.
pub(crate) fn compose_exclusion_with_persisted_peers(
    current_instance_id: &str,
    current_project_path: &str,
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
        for parked in inst.prior_tool_session_ids.values() {
            if let Some(sid) = parked
                .agent_session_id
                .as_deref()
                .filter(|sid| !sid.is_empty())
            {
                set.insert(sid.to_string());
            }
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

/// Spawn `cmd`, read stdout to EOF on a worker thread, and wait for the
/// process to exit. Kills the child if `timeout` elapses first.
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

/// Pre-assign an OpenCode session id before launch by creating the session up
/// front through a short-lived `opencode serve` process.
///
/// The caller provides the resolved host environment used by the real agent
/// launch so both processes select the same store. Any failure leaves the
/// session unowned; AoE never guesses from the shared SQLite store.
pub(crate) fn preassign_opencode_session_id(
    project_path: &str,
    environment: &[String],
) -> Option<String> {
    preassign_opencode_session_id_impl(project_path, environment)
        .map_err(|e| {
            tracing::warn!(
                target: "session.capture",
                "opencode session preassign failed ({e}); automatic capture remains disabled"
            )
        })
        .ok()
        .and_then(validated_session_id)
}

fn preassign_opencode_session_id_impl(
    project_path: &str,
    environment: &[String],
) -> Result<String> {
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
    cmd.envs(crate::session::environment::resolve_host_environment_pairs(
        environment,
    ));
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

// ─── Codex CLI session capture ────────────────────────────────────────────────

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

const CODEX_ROLLOUT_MAX_BYTES: usize = 16 * 1024 * 1024;
const CODEX_METADATA_MAX_BYTES: usize = 64 * 1024;
const CODEX_SCAN_MAX_DEPTH: usize = 4;
const CODEX_SCAN_MAX_ENTRIES: usize = 8 * 1024;
const CODEX_SCAN_MAX_CANDIDATES: usize = 4 * 1024;

fn read_limited_first_line(reader: impl Read, max_bytes: usize) -> Option<String> {
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    let mut limited = reader.take(max_bytes.saturating_add(1) as u64);
    std::io::BufRead::read_until(
        &mut std::io::BufReader::new(&mut limited),
        b'\n',
        &mut bytes,
    )
    .ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes).ok()
}

fn read_codex_metadata(root: &crate::session::AnchoredDir, relative: &Path) -> Option<String> {
    let file = root
        .open_regular(relative, CODEX_ROLLOUT_MAX_BYTES)
        .ok()??;
    if relative.extension().and_then(|e| e.to_str()) == Some("zst") {
        let decoder = zstd::stream::read::Decoder::new(file).ok()?;
        read_limited_first_line(decoder, CODEX_METADATA_MAX_BYTES)
    } else {
        read_limited_first_line(file, CODEX_METADATA_MAX_BYTES)
    }
}

fn collect_codex_sessions_anchored(
    root: &crate::session::AnchoredDir,
    relative: &Path,
    depth: usize,
    visited: &mut usize,
    entries: &mut Vec<(PathBuf, std::time::SystemTime)>,
) -> Result<()> {
    if depth > CODEX_SCAN_MAX_DEPTH || *visited >= CODEX_SCAN_MAX_ENTRIES {
        return Ok(());
    }
    let names = root.read_dir(relative, CODEX_SCAN_MAX_ENTRIES.saturating_sub(*visited))?;
    for name in names {
        *visited = visited.saturating_add(1);
        if *visited > CODEX_SCAN_MAX_ENTRIES || entries.len() >= CODEX_SCAN_MAX_CANDIDATES {
            break;
        }
        let path = relative.join(&name);
        let numeric = name
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()));
        if numeric && depth < CODEX_SCAN_MAX_DEPTH {
            if root.directory_modified(&path).ok().flatten().is_some() {
                let _ = collect_codex_sessions_anchored(root, &path, depth + 1, visited, entries);
            }
        } else if is_codex_rollout(&path) {
            if let Some(modified) = root.regular_modified(&path).ok().flatten() {
                entries.push((path, modified));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn collect_codex_sessions(
    dir: &Path,
    entries: &mut Vec<(PathBuf, std::time::SystemTime)>,
) -> Result<()> {
    let root = crate::session::AnchoredDir::open(dir)?;
    let mut relative_entries = Vec::new();
    collect_codex_sessions_anchored(&root, Path::new(""), 0, &mut 0, &mut relative_entries)?;
    entries.extend(
        relative_entries
            .into_iter()
            .map(|(relative, modified)| (dir.join(relative), modified)),
    );
    Ok(())
}

/// Poll the mounted Codex store for a post-launch rollout whose CWD matches the container.
pub(crate) fn codex_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: std::time::SystemTime,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let root = crate::session::AnchoredDir::open(&store).ok()?;
        let sessions = Path::new("sessions");
        root.directory_modified(sessions).ok().flatten()?;
        let mut entries = Vec::new();
        collect_codex_sessions_anchored(&root, sessions, 0, &mut 0, &mut entries).ok()?;
        entries.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        entries.into_iter().find_map(|(relative, modified)| {
            if modified <= capture_floor {
                return None;
            }
            let id = extract_codex_uuid_from_filename(&relative)?;
            if exclusion.contains(&id) {
                return None;
            }
            let first_line = read_codex_metadata(&root, &relative)?;
            (parse_codex_cwd_from_json(&first_line, &id).as_deref() == Some(container_cwd.as_str()))
                .then_some(id)
                .and_then(validated_session_id)
        })
    }
}

// ─── Gemini CLI session capture ───────────────────────────────────────────────

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

const GEMINI_SESSION_MAX_BYTES: usize = 8 * 1024 * 1024;
const GEMINI_METADATA_MAX_BYTES: usize = 128 * 1024;
const GEMINI_SCAN_MAX_CANDIDATES: usize = 4 * 1024;
const GEMINI_PROJECT_HASH_MAX_BYTES: usize = 128;

/// Parse the metadata of a Gemini session file (already in memory).
fn parse_gemini_session_json(content: &str) -> Option<(Option<String>, Option<String>)> {
    if content.len() > GEMINI_SESSION_MAX_BYTES {
        return None;
    }
    let extract = |v: &serde_json::Value| {
        let session_id = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .filter(|value| value.len() <= MAX_SESSION_ID_LEN)
            .map(String::from);
        let project_hash = v
            .get("projectHash")
            .and_then(|x| x.as_str())
            .filter(|value| value.len() <= GEMINI_PROJECT_HASH_MAX_BYTES)
            .map(String::from);
        (session_id, project_hash)
    };
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
        return Some(extract(&parsed));
    }
    let first_line = content.lines().next()?;
    if first_line.len() > GEMINI_METADATA_MAX_BYTES {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(first_line).ok()?;
    Some(extract(&parsed))
}

fn extract_gemini_fields_anchored(
    root: &crate::session::AnchoredDir,
    relative: &Path,
) -> Option<(Option<String>, Option<String>)> {
    let content = root
        .read_regular(relative, GEMINI_SESSION_MAX_BYTES)
        .ok()??;
    let content = String::from_utf8(content).ok()?;
    let (session_id, project_hash) = parse_gemini_session_json(&content)?;
    let session_id = session_id.or_else(|| {
        relative
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|value| value.len() <= MAX_SESSION_ID_LEN)
            .map(String::from)
    });
    Some((session_id, project_hash))
}

#[cfg(test)]
fn extract_gemini_fields(path: &std::path::Path) -> Option<(Option<String>, Option<String>)> {
    let parent = crate::session::AnchoredDir::open(path.parent()?).ok()?;
    extract_gemini_fields_anchored(&parent, Path::new(path.file_name()?))
}

/// Poll the mounted Gemini store for a post-launch chat in the exact project-hash directory.
pub(crate) fn gemini_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: std::time::SystemTime,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    use sha2::{Digest, Sha256};

    let expected_hash = Sha256::digest(container_cwd.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    move || {
        let root = crate::session::AnchoredDir::open(&store).ok()?;
        let chats = Path::new("tmp").join(&expected_hash).join("chats");
        let mut candidates = root
            .read_dir(&chats, GEMINI_SCAN_MAX_CANDIDATES)
            .ok()?
            .into_iter()
            .filter_map(|name| {
                let path = chats.join(name);
                let modified = root.regular_modified(&path).ok().flatten()?;
                let valid_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("session-"));
                let valid_extension = matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("json" | "jsonl")
                );
                (modified > capture_floor && valid_name && valid_extension)
                    .then_some((path, modified))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        candidates.into_iter().find_map(|(path, _)| {
            let (id, project_hash) = extract_gemini_fields_anchored(&root, &path)?;
            let id = id?;
            (project_hash.as_deref() == Some(expected_hash.as_str()) && !exclusion.contains(&id))
                .then_some(id)
                .and_then(validated_session_id)
        })
    }
}
// ─── Kimi Code session capture ────────────────────────────────────────────────

/// One live entry from Kimi's session index.
struct KimiSession {
    id: String,
    session_dir: String,
    work_dir: String,
}

const KIMI_INDEX_MAX_BYTES: usize = 8 * 1024 * 1024;
const KIMI_INDEX_MAX_LINE_BYTES: usize = 64 * 1024;
const KIMI_INDEX_MAX_LINES: usize = 32 * 1024;
const KIMI_INDEX_MAX_LIVE_SESSIONS: usize = 8 * 1024;
const KIMI_PATH_MAX_BYTES: usize = 16 * 1024;

fn read_kimi_session_index_anchored(
    root: &crate::session::AnchoredDir,
    relative: &Path,
) -> Result<Vec<KimiSession>> {
    let content = root
        .read_regular(relative, KIMI_INDEX_MAX_BYTES)?
        .ok_or_else(|| {
            anyhow::anyhow!("Kimi session index is missing or not a bounded regular file")
        })?;

    let mut live: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for (index, raw_line) in content.split(|byte| *byte == b'\n').enumerate() {
        if index >= KIMI_INDEX_MAX_LINES {
            anyhow::bail!("Kimi session index exceeds the line limit");
        }
        if raw_line.len() > KIMI_INDEX_MAX_LINE_BYTES {
            continue;
        }
        let Ok(line) = std::str::from_utf8(raw_line) else {
            continue;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session_id) = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|value| value.len() <= MAX_SESSION_ID_LEN)
        else {
            continue;
        };
        if value.get("deleted").and_then(|v| v.as_bool()) == Some(true) {
            live.remove(session_id);
            continue;
        }
        let (Some(session_dir), Some(work_dir)) = (
            value
                .get("sessionDir")
                .and_then(|v| v.as_str())
                .filter(|value| value.len() <= KIMI_PATH_MAX_BYTES),
            value
                .get("workDir")
                .and_then(|v| v.as_str())
                .filter(|value| value.len() <= KIMI_PATH_MAX_BYTES),
        ) else {
            continue;
        };
        if !live.contains_key(session_id) && live.len() >= KIMI_INDEX_MAX_LIVE_SESSIONS {
            continue;
        }
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

#[cfg(test)]
fn read_kimi_session_index(index_path: &Path) -> Result<Vec<KimiSession>> {
    let parent = crate::session::AnchoredDir::open(
        index_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Kimi index has no parent"))?,
    )?;
    read_kimi_session_index_anchored(
        &parent,
        Path::new(index_path.file_name().unwrap_or_default()),
    )
}

/// Strict launch-time floor. Timestamp uncertainty fails closed rather than
/// admitting a transcript that existed before this pane launched.
const KIMI_MTIME_FLOOR_SLACK_MS: f64 = 0.0;

/// Poll the mounted Kimi store for a post-launch session directory whose
/// recorded work directory matches the container workspace.
pub(crate) fn kimi_poll_fn_sandboxed_store(
    store: PathBuf,
    container_workdir: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let root = crate::session::AnchoredDir::open(&store).ok()?;
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        let sessions =
            read_kimi_session_index_anchored(&root, Path::new("session_index.jsonl")).ok()?;
        let canonical_match = canonicalize_or_raw(&container_workdir);
        let mut candidates = sessions
            .into_iter()
            .filter(|session| !exclusion.contains(&session.id))
            .filter(|session| canonicalize_or_raw(&session.work_dir) == canonical_match)
            .filter_map(|session| {
                let leaf = Path::new(&session.session_dir).file_name()?;
                let mtime = root
                    .directory_modified(&Path::new("sessions").join(leaf))
                    .ok()
                    .flatten()?;
                let mtime_ms = crate::util::system_time_to_ms(mtime);
                ((mtime_ms as f64) + KIMI_MTIME_FLOOR_SLACK_MS >= launch_time_ms)
                    .then_some((session.id, mtime_ms))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
        candidates
            .into_iter()
            .next()
            .map(|candidate| candidate.0)
            .and_then(validated_session_id)
    }
}
/// Strict launch-time floor. Timestamp uncertainty fails closed rather than
/// admitting a transcript that existed before this pane launched.
const PRIME_AGENT_MTIME_FLOOR_SLACK_MS: f64 = 0.0;

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

/// Maximum number of Prime Agent transcript entries inspected per poll. An
/// instance-private store should contain very few files; exceeding this bound
/// fails closed instead of turning a poll into attacker-controlled work.
const PRIME_AGENT_MAX_SESSION_FILES: usize = 256;

/// Scan `<prime-agent home>/sessions/*.jsonl` through one anchored root.
/// Intermediate and leaf symlinks, non-regular files, oversized headers, and
/// stores above the entry cap are rejected. The launch-floor mtime comes from
/// the opened descriptor, so a path replacement cannot swap its timestamp.
fn scan_prime_agent_sessions(store: &Path) -> Vec<PrimeAgentSession> {
    let Ok(root) = crate::session::AnchoredDir::open(store) else {
        return Vec::new();
    };
    let Ok(names) = root.read_dir(
        Path::new("sessions"),
        PRIME_AGENT_MAX_SESSION_FILES.saturating_add(1),
    ) else {
        return Vec::new();
    };
    if names.len() > PRIME_AGENT_MAX_SESSION_FILES {
        return Vec::new();
    }

    let mut sessions = Vec::new();
    for name in names {
        let path = Path::new(&name);
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let relative = Path::new("sessions").join(&name);
        let Ok(Some(file)) = root.open_regular(&relative, usize::MAX) else {
            continue;
        };
        let mtime_ms = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map(crate::util::system_time_to_ms)
            .unwrap_or(0);
        let mut header = Vec::with_capacity(4096);
        let read = std::io::BufReader::new(file)
            .take(PRIME_AGENT_HEADER_SCAN_BYTES.saturating_add(1))
            .read_until(b'\n', &mut header);
        if read.is_err()
            || header.is_empty()
            || u64::try_from(header.len()).unwrap_or(u64::MAX) > PRIME_AGENT_HEADER_SCAN_BYTES
        {
            continue;
        }
        let Ok(header) = serde_json::from_slice::<serde_json::Value>(&header) else {
            continue;
        };
        if header.get("type").and_then(|value| value.as_str()) != Some("session") {
            continue;
        }
        let (Some(id), Some(cwd)) = (
            header.get("id").and_then(|value| value.as_str()),
            header.get("cwd").and_then(|value| value.as_str()),
        ) else {
            continue;
        };
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

/// Poll the mounted Prime Agent store for a post-launch transcript whose CWD
/// matches the container workspace.
pub(crate) fn prime_agent_poll_fn_sandboxed_store(
    store: PathBuf,
    container_workdir: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        select_prime_agent_session(
            scan_prime_agent_sessions(&store),
            &container_workdir,
            &exclusion,
            Some(launch_time_ms),
        )
        .map_err(|error| {
            tracing::debug!(target: "session.capture", "sandbox Prime Agent capture failed: {error}")
        })
        .ok()
        .and_then(validated_session_id)
    }
}

// ─── Hermes session capture ───────────────────────────────────────────────────

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
fn normalize_hermes_signal(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
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

const HERMES_MAX_ROWS: usize = 4 * 1024;
const HERMES_MAX_SCHEMA_COLUMNS: usize = 1024;
const HERMES_SIGNAL_MAX_BYTES: usize = 16 * 1024;

/// Read active CLI session rows from Hermes's SQLite state database.
///
/// Returns the full active CLI set, newest first, with each row's `cwd` and
/// `git_repo_root` when the schema has those columns (NULL literal
/// otherwise). An `Err` means the DB is unreadable (missing, locked, schema
/// mismatch); the poller will retry on the next tick.
fn read_hermes_sessions_from_sqlite(
    db_path: &Path,
    started_after: Option<f64>,
) -> Result<HermesSessionScan> {
    use rusqlite::{Connection, OpenFlags};

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
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
        for (index, col) in cols.enumerate() {
            if index >= HERMES_MAX_SCHEMA_COLUMNS {
                anyhow::bail!("Hermes sessions table exceeds the column limit");
            }
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
         AND (?1 IS NULL OR started_at > ?1) \
         AND length(id) <= ?2 \
         AND ({cwd_expr} IS NULL OR length({cwd_expr}) <= ?3) \
         AND ({root_expr} IS NULL OR length({root_expr}) <= ?3) \
         ORDER BY started_at DESC, id DESC LIMIT ?4"
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("Hermes sessions table missing or schema mismatch")?;

    let rows = stmt
        .query_map(
            rusqlite::params![
                started_after,
                MAX_SESSION_ID_LEN as i64,
                HERMES_SIGNAL_MAX_BYTES as i64,
                (HERMES_MAX_ROWS + 1) as i64
            ],
            |row| {
                let id: String = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                let root: Option<String> = row.get(2)?;
                Ok(HermesSessionRow {
                    id,
                    cwd: normalize_hermes_signal(cwd),
                    git_repo_root: normalize_hermes_signal(root),
                })
            },
        )
        .context("Failed to query Hermes sessions table")?;

    let mut out: Vec<HermesSessionRow> = Vec::new();
    for row in rows {
        out.push(row.context("Failed to read Hermes session row")?);
    }
    if out.len() > HERMES_MAX_ROWS {
        anyhow::bail!("Hermes active session count exceeds the row limit");
    }

    Ok(HermesSessionScan {
        rows: out,
        signal_columns_present: has_cwd || has_git_repo_root,
    })
}
/// Poll the mounted Hermes store for a post-launch, project-scoped session.
pub(crate) fn hermes_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: std::time::SystemTime,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let started_after = capture_floor
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(f64::MAX);
    move || {
        let root = crate::session::AnchoredDir::open(&store).ok()?;
        let db_relative = Path::new("state.db");
        if !root.regular_exists(db_relative) {
            return None;
        }
        let scan =
            read_hermes_sessions_from_sqlite(&root.path().join(db_relative), Some(started_after))
                .ok()?;
        let exclusion = compose_exclusion(&instance_id, &extra_excludes);
        select_hermes_session_id(&scan, &container_cwd, &exclusion)
            .ok()
            .and_then(validated_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn open_fifo_guard(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK);
        options.open(path).unwrap()
    }

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
    #[serial_test::serial]
    fn parked_conversation_exclusion_survives_alias_config_changes() {
        const PROFILE: &str = "capture-parked-alias-test";
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(PROFILE);
        let mut config = crate::session::Config::default();
        config
            .session
            .agent_detect_as
            .insert("claude-personal".to_string(), "claude".to_string());
        crate::tmux::status_rules::install_from_config(PROFILE, &config);

        let project = "/tmp/capture-parked-alias";
        let parked_sid = "88888888-8888-4888-8888-888888888888";
        let mut peer = crate::session::Instance::new("peer", project);
        peer.source_profile = PROFILE.to_string();
        peer.tool = "codex".to_string();
        peer.prior_tool_session_ids.insert(
            "claude-personal".to_string(),
            crate::session::instance::PriorToolSession {
                agent_session_id: Some(parked_sid.to_string()),
                acp_session_id: None,
            },
        );
        let storage = crate::session::Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|instances, groups| {
                *instances = vec![peer.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(instances, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
        // The peer row reloads without `source_profile`, and the alias may be
        // removed or retargeted before it swaps back. Ownership must survive
        // both facts rather than being reconstructed from current config.
        crate::tmux::status_rules::install_from_config(PROFILE, &crate::session::Config::default());

        let exclusions =
            compose_exclusion_with_persisted_peers("current", project, PROFILE, &HashSet::new());
        assert!(
            exclusions.contains(parked_sid),
            "a conversation parked under an alias belongs to the same built-in store"
        );
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
        assert!(!is_valid_session_id("-looks-like-an-option"));
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

    #[cfg(unix)]
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
    fn test_hermes_session_id_format_valid() {
        assert!(is_valid_session_id("20260429_193246_adcddd"));
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

    #[cfg(unix)]
    #[test]
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

        let scanned = scan_prime_agent_sessions(tmp.path());
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

        assert!(scan_prime_agent_sessions(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
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

        assert!(scan_prime_agent_sessions(tmp.path()).is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn test_scan_prime_agent_sessions_rejects_symlinked_sessions_directory() {
        use std::os::unix::fs::symlink;

        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_prime_session(outside.path(), "peer.jsonl", "peer-id", "/workspace");
        symlink(outside.path(), store.path().join("sessions")).unwrap();

        assert!(scan_prime_agent_sessions(store.path()).is_empty());
    }

    #[test]
    fn test_scan_prime_agent_sessions_fails_closed_above_entry_cap() {
        let store = tempfile::tempdir().unwrap();
        let sessions = store.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        for index in 0..=PRIME_AGENT_MAX_SESSION_FILES {
            std::fs::write(sessions.join(format!("{index:04}.txt")), b"noise").unwrap();
        }
        write_prime_session(&sessions, "valid.jsonl", "valid-id", "/workspace");

        assert!(scan_prime_agent_sessions(store.path()).is_empty());
    }

    fn capture_floor(seconds: u64) -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn set_mtime_seconds(path: &Path, seconds: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
    }

    #[test]
    fn sandbox_codex_poller_only_claims_post_launch_matching_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions/2026/08/23");
        std::fs::create_dir_all(&sessions).unwrap();
        let stale_id = "11111111-2222-4333-8444-555555555555";
        let fresh_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        for (name, id, cwd, mtime) in [
            ("stale", stale_id, "/workspace", 1_000),
            (
                "wrong",
                "99999999-8888-4777-8666-555555555555",
                "/other",
                4_000,
            ),
            ("fresh", fresh_id, "/workspace", 3_000),
        ] {
            let path = sessions.join(format!("rollout-{name}-{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}
"#
                ),
            )
            .unwrap();
            set_mtime_seconds(&path, mtime);
        }

        let poll = codex_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(2_000),
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some(fresh_id));
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_codex_poller_skips_hostile_artifacts_without_blocking() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let good_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let good = sessions.join(format!("rollout-good-{good_id}.jsonl"));
        std::fs::write(
            &good,
            format!(
                r#"{{"payload":{{"id":"{good_id}","cwd":"/workspace"}}}}
"#
            ),
        )
        .unwrap();
        set_mtime_seconds(&good, 3_000);

        let linked_id = "11111111-2222-4333-8444-555555555555";
        let outside_file = outside
            .path()
            .join(format!("rollout-linked-{linked_id}.jsonl"));
        std::fs::write(
            &outside_file,
            format!(
                r#"{{"payload":{{"id":"{linked_id}","cwd":"/workspace"}}}}
"#
            ),
        )
        .unwrap();
        symlink(
            &outside_file,
            sessions.join(outside_file.file_name().unwrap()),
        )
        .unwrap();
        std::fs::create_dir(outside.path().join("06")).unwrap();
        symlink(outside.path().join("06"), sessions.join("2026")).unwrap();

        let fifo_id = "22222222-3333-4444-8555-666666666666";
        let fifo = sessions.join(format!("rollout-fifo-{fifo_id}.jsonl"));
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let _fifo_guard = open_fifo_guard(&fifo);
        let oversized_id = "33333333-4444-4555-8666-777777777777";
        let mut oversized = format!(
            r#"{{"payload":{{"id":"{oversized_id}","cwd":"/workspace"}}}}
"#
        )
        .into_bytes();
        oversized.resize(CODEX_ROLLOUT_MAX_BYTES + 1, b' ');
        std::fs::write(
            sessions.join(format!("rollout-large-{oversized_id}.jsonl")),
            oversized,
        )
        .unwrap();
        let bomb_id = "44444444-5555-4666-8777-888888888888";
        let bomb = zstd::stream::encode_all(
            std::io::Cursor::new(vec![b' '; CODEX_METADATA_MAX_BYTES + 1]),
            1,
        )
        .unwrap();
        std::fs::write(
            sessions.join(format!("rollout-bomb-{bomb_id}.jsonl.zst")),
            bomb,
        )
        .unwrap();

        let poll = codex_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(100),
            HashSet::new(),
        );
        let started = Instant::now();
        assert_eq!(poll().as_deref(), Some(good_id));
        std::fs::remove_file(good).unwrap();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn sandbox_gemini_poller_only_claims_post_launch_matching_chat() {
        use sha2::{Digest, Sha256};

        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/workspace";
        let hash = Sha256::digest(cwd.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let chats = tmp.path().join("tmp").join(&hash).join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let stale_id = "11111111-2222-4333-8444-555555555555";
        let fresh_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        for (name, id, project_hash, mtime) in [
            ("stale", stale_id, hash.as_str(), 1_000),
            (
                "wrong",
                "99999999-8888-4777-8666-555555555555",
                "wrong",
                4_000,
            ),
            ("fresh", fresh_id, hash.as_str(), 3_000),
        ] {
            let path = chats.join(format!("session-{name}.json"));
            std::fs::write(
                &path,
                format!(r#"{{"sessionId":"{id}","projectHash":"{project_hash}"}}"#),
            )
            .unwrap();
            set_mtime_seconds(&path, mtime);
        }

        let poll = gemini_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            cwd.to_string(),
            "current".to_string(),
            capture_floor(2_000),
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some(fresh_id));
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_gemini_poller_skips_symlinks_fifo_and_oversized_json() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = "/workspace";
        let hash = Sha256::digest(cwd.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let chats = tmp.path().join("tmp").join(&hash).join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let good = chats.join("session-good.json");
        std::fs::write(
            &good,
            format!(r#"{{"sessionId":"gemini_good","projectHash":"{hash}"}}"#),
        )
        .unwrap();
        let outside_file = outside.path().join("outside.json");
        std::fs::write(
            &outside_file,
            format!(r#"{{"sessionId":"gemini_linked","projectHash":"{hash}"}}"#),
        )
        .unwrap();
        symlink(&outside_file, chats.join("session-linked.json")).unwrap();
        let fifo = chats.join("session-pipe.json");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let _fifo_guard = open_fifo_guard(&fifo);
        std::fs::write(
            chats.join("session-large.json"),
            vec![b' '; GEMINI_SESSION_MAX_BYTES + 1],
        )
        .unwrap();

        let poll = gemini_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            cwd.to_string(),
            "current".to_string(),
            capture_floor(100),
            HashSet::new(),
        );
        let started = Instant::now();
        assert_eq!(poll().as_deref(), Some("gemini_good"));
        std::fs::remove_file(good).unwrap();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));

        let intermediate = tempfile::tempdir().unwrap();
        symlink(outside.path(), intermediate.path().join("tmp")).unwrap();
        let poll = gemini_poll_fn_sandboxed_store(
            intermediate.path().to_path_buf(),
            cwd.to_string(),
            "current".to_string(),
            capture_floor(100),
            HashSet::new(),
        );
        assert_eq!(poll(), None);
    }

    #[test]
    fn sandbox_hermes_poller_only_claims_post_launch_matching_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);",
        )
        .unwrap();
        for (id, started_at, cwd) in [
            ("hermes_stale", 1_000.0, "/workspace"),
            ("hermes_wrong", 4_000.0, "/other"),
            ("hermes_fresh", 3_000.0, "/workspace"),
        ] {
            conn.execute(
                "INSERT INTO sessions VALUES (?1, 'cli', ?2, NULL, ?3, NULL)",
                rusqlite::params![id, started_at, cwd],
            )
            .unwrap();
        }
        drop(conn);

        let poll = hermes_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(2_000),
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some("hermes_fresh"));
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_hermes_poller_refuses_symlink_fifo_and_excess_rows() {
        use std::os::unix::fs::symlink;

        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_db = outside.path().join("state.db");
        let conn = rusqlite::Connection::open(&outside_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT); \
             INSERT INTO sessions VALUES ('linked', 'cli', 10, NULL, '/workspace', NULL);",
        )
        .unwrap();
        drop(conn);
        symlink(&outside_db, store.path().join("state.db")).unwrap();
        let poll = hermes_poll_fn_sandboxed_store(
            store.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(0),
            HashSet::new(),
        );
        assert_eq!(poll(), None);
        std::fs::remove_file(store.path().join("state.db")).unwrap();
        let fifo = store.path().join("state.db");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let fifo_guard = open_fifo_guard(&fifo);
        let started = Instant::now();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(fifo_guard);
        std::fs::remove_file(&fifo).unwrap();

        let conn = rusqlite::Connection::open(store.path().join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);",
        )
        .unwrap();
        let transaction = conn.unchecked_transaction().unwrap();
        for index in 0..=HERMES_MAX_ROWS {
            transaction
                .execute(
                    "INSERT INTO sessions VALUES (?1, 'cli', ?2, NULL, '/workspace', NULL)",
                    rusqlite::params![format!("hermes_{index}"), index as f64 + 1.0],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(conn);
        assert_eq!(poll(), None);
    }

    #[test]
    fn sandbox_kimi_poller_only_claims_post_launch_matching_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        for (leaf, mtime) in [
            ("stale", 1_000),
            ("boundary", 2_000),
            ("wrong", 4_000),
            ("fresh", 3_000),
        ] {
            let path = sessions.join(leaf);
            std::fs::create_dir(&path).unwrap();
            set_mtime_seconds(&path, mtime);
        }
        std::fs::write(
            tmp.path().join("session_index.jsonl"),
            concat!(
                r#"{"sessionId":"kimi_stale","sessionDir":"/sessions/stale","workDir":"/workspace"}"#,
                "\n",
                r#"{"sessionId":"kimi_boundary","sessionDir":"/sessions/boundary","workDir":"/workspace"}"#,
                "\n",
                r#"{"sessionId":"kimi_wrong","sessionDir":"/sessions/wrong","workDir":"/other"}"#,
                "\n",
                r#"{"sessionId":"kimi_fresh","sessionDir":"/sessions/fresh","workDir":"/workspace"}"#,
                "\n",
            ),
        )
        .unwrap();

        let poll = kimi_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            2_000_001.0,
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some("kimi_fresh"));
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_kimi_poller_skips_hostile_index_and_session_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let good = sessions.join("good");
        std::fs::create_dir(&good).unwrap();
        std::fs::create_dir(outside.path().join("linked")).unwrap();
        symlink(outside.path().join("linked"), sessions.join("linked")).unwrap();
        let index = tmp.path().join("session_index.jsonl");
        let index_content = concat!(
            r#"{"sessionId":"kimi_linked","sessionDir":"/sessions/linked","workDir":"/workspace"}"#,
            "\n",
            r#"{"sessionId":"kimi_good","sessionDir":"/sessions/good","workDir":"/workspace"}"#,
            "\n",
        );
        std::fs::write(&index, index_content).unwrap();
        let poll = kimi_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            0.0,
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some("kimi_good"));
        std::fs::remove_dir(good).unwrap();
        assert_eq!(poll(), None);

        std::fs::remove_file(&index).unwrap();
        let outside_index = outside.path().join("session_index.jsonl");
        std::fs::write(&outside_index, index_content).unwrap();
        symlink(&outside_index, &index).unwrap();
        assert_eq!(poll(), None);
        std::fs::remove_file(&index).unwrap();
        nix::unistd::mkfifo(
            &index,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let fifo_guard = open_fifo_guard(&index);
        let started = Instant::now();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(fifo_guard);
        std::fs::remove_file(&index).unwrap();
        std::fs::write(&index, vec![b' '; KIMI_INDEX_MAX_BYTES + 1]).unwrap();
        assert_eq!(poll(), None);
    }

    #[test]
    fn sandbox_prime_poller_only_claims_post_launch_matching_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        for (name, id, cwd, mtime) in [
            ("stale", "prime_stale", "/workspace", 1_000),
            ("boundary", "prime_boundary", "/workspace", 2_000),
            ("wrong", "prime_wrong", "/other", 4_000),
            ("fresh", "prime_fresh", "/workspace", 3_000),
        ] {
            let path = write_prime_session(&sessions, &format!("{name}.jsonl"), id, cwd);
            set_mtime_seconds(&path, mtime);
        }

        let poll = prime_agent_poll_fn_sandboxed_store(
            tmp.path().to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            2_000_001.0,
            HashSet::new(),
        );
        assert_eq!(poll().as_deref(), Some("prime_fresh"));
    }
}

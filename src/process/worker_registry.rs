//! On-disk registry of detached structured view worker processes.
//!
//! Each running structured view worker has a JSON file at
//! `<app_dir>/acp-workers/<session_id>.json` describing how to dial it
//! and who owns the process. The directory is the source of truth across
//! `aoe serve` restarts: when serve starts, it scans the directory, dials
//! every live worker, and only spawns a fresh worker for sessions that
//! have no registry entry (or a dead one).
//!
//! The worker process itself (the `aoe __acp-runner` shim) writes the
//! file on startup and removes it on graceful exit; `Supervisor::shutdown`
//! and the stale-sweep on serve startup remove it for crashed runners.
//!
//! File mode is 0600 because `provider_env_keys` and `socket_path` may
//! leak metadata about which agents/providers a user runs.
//!
//! Layout note: the runner and daemon both mutate entries. Writes and owned
//! deletion serialize on a per-session lock so a superseded runner cannot
//! unlink a replacement record between its ownership check and cleanup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::util::now_secs;

// Generic worker-subprocess plumbing now lives in `process::worker`; the
// registry is the ACP consumer of it. Re-exported so the names referenced
// across the ACP code (and its tests) keep resolving here.
pub use crate::process::worker::{is_pid_alive, validate_id as validate_session_id};

/// Generation of the runner protocol and ownership semantics this daemon speaks.
/// Generation 3 uses the typed control socket, attachment-scoped correlations,
/// and ownership-aware teardown. Earlier generations cannot be attached safely.
///
/// This is deliberately separate from `is_record_live`: a wrong-generation
/// process is still live and must be reaped before its replacement starts.
/// Build-stale workers of the current generation remain attachable and may
/// drain an in-flight turn before replacement.
pub const RUNNER_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub runner_version: u32,
    /// Binary build identity (`build_info::BUILD_VERSION`) of the
    /// `aoe __acp-runner` process that wrote this record, e.g.
    /// `"1.9.5+g7f31a9c42e01"`. Distinct from `runner_version`, which is
    /// the runner's protocol/topology generation (#2977): two runners on the
    /// same generation can differ in build and vice versa, so the daemon
    /// tracks them as independent staleness axes. The daemon compares this
    /// against its own `BUILD_VERSION` to detect a worker left running on an
    /// older binary after `aoe update` and respawn it (see #1754). Defaulted
    /// on load for legacy records that pre-date this field; the empty string
    /// compares unequal to any current build, forcing a one-time respawn.
    #[serde(default)]
    pub build_version: String,
    pub session_id: String,
    /// PID of the `aoe __acp-runner` process. Used by the stale-sweep
    /// to decide whether the registry entry corresponds to a live owner.
    pub pid: u32,
    pub socket_path: PathBuf,
    /// Binary command name that the runner was invoked with
    /// (e.g. `"claude-agent-acp"`, `"codex-acp"`). Surfaced in
    /// `aoe acp ps`, logs, and the doctor's install-hint lookup.
    /// NOT the registry key; use `agent_key` to resolve a profile.
    pub agent_name: String,
    /// Registry key for the agent (e.g. `"claude"`, `"codex"`,
    /// `"opencode"`). Drives `acp::agent_profiles::resolve` and any
    /// other per-agent gate keyed on the registry name. Defaulted on
    /// load for legacy records that pre-date this field; the empty
    /// string falls back to `DEFAULT_AGENT_PROFILE` at the call site,
    /// which is the safest behavior for an unknown agent.
    #[serde(default)]
    pub agent_key: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub additional_dirs: Vec<PathBuf>,
    /// Keys (not values) of provider_env passed through at spawn. Lets
    /// the reconciler observe which provider auth was configured for the
    /// session without re-reading every entry on every tick.
    pub provider_env_keys: Vec<String>,
    /// Cached ACP session id assigned by the agent on first `session/new`.
    /// On reattach, the daemon sends `session/load <stored_acp_session_id>`
    /// to resume the agent-side transcript.
    pub stored_acp_session_id: Option<String>,
    /// Profile the session was created under. Persisted so reattach can
    /// re-resolve sandbox env (`terminal/create` env entries) against the
    /// same profile the session originally used, instead of silently
    /// falling back to the global default profile. Defaulted on load for
    /// legacy records that pre-date this field; an absent value falls
    /// back to the default profile, matching pre-persistence behavior.
    #[serde(default)]
    pub source_profile: Option<String>,
    pub started_at: u64,
    pub last_attached_at: Option<u64>,
    pub detached_at: Option<u64>,
}

impl WorkerRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        pid: u32,
        socket_path: PathBuf,
        agent_name: String,
        agent_key: String,
        cwd: PathBuf,
        model: Option<String>,
        additional_dirs: Vec<PathBuf>,
        provider_env_keys: Vec<String>,
        stored_acp_session_id: Option<String>,
        source_profile: Option<String>,
    ) -> Self {
        Self {
            runner_version: RUNNER_VERSION,
            build_version: crate::build_info::BUILD_VERSION.to_string(),
            session_id,
            pid,
            socket_path,
            agent_name,
            agent_key,
            cwd,
            model,
            additional_dirs,
            provider_env_keys,
            stored_acp_session_id,
            source_profile,
            started_at: now_secs(),
            last_attached_at: None,
            detached_at: None,
        }
    }
}

/// Directory holding worker JSON files, log files, and the per-session
/// unix sockets. Auto-created on first access.
pub fn workers_dir() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?.join("acp-workers");
    crate::process::worker::ensure_dir(&dir)?;
    Ok(dir)
}

/// `<workers_dir>/<session_id>.json`.
pub fn record_path(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::record_path(&workers_dir()?, session_id)
}

/// `<workers_dir>/<session_id>.sock`. Caller computes this once and threads
/// the same path into both the runner spawn and the daemon connect.
pub fn socket_path_for(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::socket_path(&workers_dir()?, session_id)
}

/// `<workers_dir>/<session_id>.log` is the runner-side stderr drain
/// consumed by `aoe acp logs --session <id>`.
pub fn log_path_for(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::log_path(&workers_dir()?, session_id)
}

/// Sentinel file `<workers_dir>/<session_id>.restart`. Written by
/// `aoe acp restart` BEFORE the registry delete + SIGTERM so the
/// daemon's reaper can distinguish a restart-driven teardown from
/// `aoe acp stop|kill` and:
///   - emit `Stopped { reason: "restart_pending" }` instead of
///     `user_stopped` so the UI shows a "Restarting…" banner without
///     the "Reconnect" button (the daemon will respawn shortly);
///   - signal the reconciler to clear the `attempted` set for this id
///     so the next 2s tick actually spawns a fresh worker.
pub fn restart_marker_path(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::restart_marker_path(&workers_dir()?, session_id)
}

/// Best-effort write of an empty restart-pending marker. Called by the
/// CLI's `aoe acp restart` before deleting the registry entry. The
/// file's existence is the signal; its contents are irrelevant.
pub fn mark_restart_pending(session_id: &str) {
    let Ok(path) = restart_marker_path(session_id) else {
        return;
    };
    let _ = std::fs::write(&path, b"");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Returns `true` if the marker existed (and was deleted). Caller uses
/// the boolean to pick the publish reason; defense-in-depth removes the
/// file so a leaked marker doesn't poison the next spawn.
pub fn take_restart_marker(session_id: &str) -> bool {
    let Ok(path) = restart_marker_path(session_id) else {
        return false;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

/// Atomic write (temp + rename) with 0600 perms. The per-session lock also
/// prevents an ownership-checked cleanup from racing the final rename.
pub fn save(record: &WorkerRecord) -> Result<()> {
    with_registry_lock(&record.session_id, || save_unlocked(record))
}

fn save_unlocked(record: &WorkerRecord) -> Result<()> {
    let dir = workers_dir()?;
    let final_path = dir.join(format!("{}.json", record.session_id));
    let tmp_path = dir.join(format!("{}.json.tmp", record.session_id));
    let bytes = serde_json::to_vec_pretty(record).context("serializing worker record")?;
    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("writing tmp record at {}", tmp_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("renaming tmp record to {}", final_path.display()))?;
    Ok(())
}

fn with_registry_lock<T>(session_id: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    validate_session_id(session_id)?;
    let lock_path = workers_dir()?.join(format!("{session_id}.lock"));
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening worker registry lock {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = lock_file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    lock_file
        .lock_exclusive()
        .with_context(|| format!("locking worker registry entry {session_id}"))?;
    let result = operation();
    let unlock = fs2::FileExt::unlock(&lock_file)
        .with_context(|| format!("unlocking worker registry entry {session_id}"));
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub fn load(session_id: &str) -> Result<Option<WorkerRecord>> {
    let path = record_path(session_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_slice::<WorkerRecord>(&bytes) {
        Ok(record) => Ok(Some(record)),
        Err(e) => {
            warn!(
                target: "acp.registry",
                path = %path.display(),
                "failed to parse worker record: {e}; treating as missing"
            );
            Ok(None)
        }
    }
}
fn load_strict_unlocked(session_id: &str) -> Result<Option<WorkerRecord>> {
    let path = record_path(session_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn list() -> Result<Vec<WorkerRecord>> {
    let dir = workers_dir()?;
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        match serde_json::from_slice::<WorkerRecord>(&bytes) {
            Ok(rec) => out.push(rec),
            Err(e) => {
                warn!(
                    target: "acp.registry",
                    path = %path.display(),
                    "skipping unparseable worker record: {e}"
                );
            }
        }
    }
    Ok(out)
}

/// Remove the JSON entry and runner sockets. Non-empty logs remain available
/// for post-mortem inspection; empty logs are swept.
pub fn delete(session_id: &str) -> Result<()> {
    with_registry_lock(session_id, || delete_unlocked(session_id))
}

fn delete_unlocked(session_id: &str) -> Result<()> {
    if let Ok(path) = record_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
    if let Ok(path) = socket_path_for(session_id) {
        remove_runner_sockets(&path);
    }
    if let Ok(path) = log_path_for(session_id) {
        if matches!(std::fs::metadata(&path), Ok(metadata) if metadata.len() == 0) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// Atomically delete registry artifacts only while the record names owner_pid.
/// A replacement save uses the same lock, so it either lands before this check
/// and is preserved or lands after cleanup and remains.
pub fn delete_if_owned(session_id: &str, owner_pid: u32) -> Result<bool> {
    with_registry_lock(session_id, || match load_strict_unlocked(session_id)? {
        Some(record) if record.pid == owner_pid => {
            delete_unlocked(session_id)?;
            Ok(true)
        }
        Some(record) => {
            debug!(
                target: "acp.registry",
                session = %session_id,
                owner_pid,
                current_pid = record.pid,
                "skipping cleanup owned by a replacement runner"
            );
            Ok(false)
        }
        None => Ok(false),
    })
}

fn update_if_owned(
    session_id: &str,
    owner_pid: u32,
    update: impl FnOnce(&mut WorkerRecord),
) -> Result<bool> {
    with_registry_lock(session_id, || {
        let Some(mut record) = load_strict_unlocked(session_id)? else {
            return Ok(false);
        };
        if record.pid != owner_pid {
            return Ok(false);
        }
        update(&mut record);
        save_unlocked(&record)?;
        Ok(true)
    })
}

fn delete_if_absent(session_id: &str) -> Result<bool> {
    with_registry_lock(session_id, || {
        if load_strict_unlocked(session_id)?.is_some() {
            return Ok(false);
        }
        delete_unlocked(session_id)?;
        Ok(true)
    })
}

/// Update the `last_attached_at` field in place while the caller owns
/// the record. Best-effort because the timestamp is only observability data.
pub fn mark_attached(session_id: &str, owner_pid: u32) {
    if let Err(error) = update_if_owned(session_id, owner_pid, |record| {
        record.last_attached_at = Some(now_secs());
        record.detached_at = None;
    }) {
        debug!(
            target: "acp.registry",
            session = %session_id,
            "failed to update last_attached_at: {error}"
        );
    }
}

pub fn mark_detached(session_id: &str, owner_pid: u32) {
    if let Err(error) = update_if_owned(session_id, owner_pid, |record| {
        record.detached_at = Some(now_secs());
    }) {
        debug!(
            target: "acp.registry",
            session = %session_id,
            "failed to update detached_at: {error}"
        );
    }
}

/// Durably update the ACP session id while `owner_pid` still owns the
/// record. The runner calls this before exposing session establishment.
pub fn update_stored_acp_session_id(session_id: &str, owner_pid: u32, acp_id: &str) -> Result<()> {
    anyhow::ensure!(!acp_id.is_empty(), "ACP session id must not be empty");
    let updated = update_if_owned(session_id, owner_pid, |record| {
        record.stored_acp_session_id = Some(acp_id.to_string());
    })?;
    anyhow::ensure!(updated, "runner no longer owns its registry record");
    Ok(())
}

/// Probe the recorded socket path. A worker registry entry is "live"
/// only if both the PID is alive AND the socket file still exists; a
/// stale entry where the runner died before deleting its files would
/// otherwise let attach hang on a missing socket.
///
/// Defense-in-depth for PID reuse: it's possible (though rare) for a
/// runner to die uncleanly, leave the socket file behind, and have its
/// PID immediately recycled by an unrelated process. The (pid_alive +
/// socket_exists) pair survives that case in almost all scenarios
/// because the unrelated process is exceedingly unlikely to be
/// listening on the same socket path. As a third layer, the daemon's
/// attach handshake (`AcpClient::attach` -> `initialize`) rejects any
/// peer that doesn't speak ACP within the 3s reconciler timeout, so a
/// truly unlucky PID/socket collision still falls back to a fresh
/// spawn rather than wedging the session.
pub fn is_record_live(rec: &WorkerRecord) -> bool {
    is_pid_alive(rec.pid) && socket_exists(&expected_socket(rec))
}

/// The socket a record of this generation actually binds. Generation 1 used
/// the legacy raw relay path; generation 2 and later use the typed control
/// sibling. The base path remains in the record for stable path derivation.
fn expected_socket(rec: &WorkerRecord) -> std::path::PathBuf {
    if rec.runner_version >= 2 {
        crate::process::worker::control_socket_sibling(&rec.socket_path)
    } else {
        rec.socket_path.clone()
    }
}

/// Remove the current control socket and the legacy raw relay path.
///
/// One helper rather than the same two lines at each teardown site, so
/// "which sockets does a runner of generation N own" has a single answer to
/// update when a future generation changes the set.
pub(crate) fn remove_runner_sockets(socket_path: &Path) {
    for path in [
        socket_path.to_path_buf(),
        crate::process::worker::control_socket_sibling(socket_path),
    ] {
        let _ = std::fs::remove_file(&path);
    }
}

/// Create the socket file a record of the CURRENT generation must have for
/// [`is_record_live`] to see it, so a fixture does not have to know which
/// path that is. Test-only: production runners bind a real listener.
#[cfg(test)]
pub(crate) fn touch_live_socket(socket_path: &Path) {
    let rec_shaped = WorkerRecord {
        runner_version: RUNNER_VERSION,
        socket_path: socket_path.to_path_buf(),
        ..WorkerRecord::new(
            "probe".into(),
            0,
            socket_path.to_path_buf(),
            String::new(),
            String::new(),
            PathBuf::new(),
            None,
            vec![],
            vec![],
            None,
            None,
        )
    };
    std::fs::write(expected_socket(&rec_shaped), b"").expect("touch live socket");
}

/// Whether the worker generation is attach-compatible with this daemon. A
/// mismatch is live but incompatible: reap it before starting a replacement.
pub fn is_runner_current(rec: &WorkerRecord) -> bool {
    rec.runner_version == RUNNER_VERSION
}

/// Whether the worker's recorded binary build matches the running
/// daemon's. A live-but-stale worker (this returns `false`) is still
/// "live" by `is_record_live`; the reconciler keeps it for any in-flight
/// turn and respawns it on the current binary at the next idle boundary,
/// rather than treating a version mismatch as death. See #1754.
///
/// Build identity is NOT folded into `is_record_live` on purpose: doing
/// so would make a busy stale worker look dead and push the reconciler
/// toward orphaning its in-flight turn.
pub fn is_build_current(rec: &WorkerRecord) -> bool {
    rec.build_version == crate::build_info::BUILD_VERSION
}

/// The ACP worker state ladder shared by `aoe ps --acp` and the deprecated
/// `aoe acp ps`: `dead` when the runner is not live; `detached` when it has
/// detached and has not re-attached since; `attached` otherwise. `live` is
/// the caller's [`is_record_live`] result, threaded in so a caller that
/// already computed it does not probe the socket twice.
pub(crate) fn worker_state_label(rec: &WorkerRecord, live: bool) -> &'static str {
    if !live {
        "dead"
    } else if rec
        .detached_at
        .is_some_and(|detached| rec.last_attached_at.unwrap_or(0) <= detached)
    {
        "detached"
    } else {
        "attached"
    }
}

fn socket_exists(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Resolve the runner's PID from whichever source is still legible: the
/// on-disk record first, then (only when `load` returns `Err`, not when
/// it returns `Ok(None)`) `SO_PEERCRED` on the live socket. The
/// distinction matters: `Ok(None)` means the runner is already gone, so
/// falling through to the socket would just probe a stale inode; `Err`
/// means we lost the primary channel and must reach for the secondary
/// before the runner escapes both SIGTERM and the shutdown wait. `load`
/// returns `Err` only for true I/O failures (permissions, wrong file
/// type, transient); JSON parse errors are coerced to `Ok(None)`
/// upstream. See #2102.
pub fn pid_source_for(session_id: &str) -> Option<u32> {
    match load(session_id) {
        Ok(Some(record)) => (record.pid > 0).then_some(record.pid),
        Ok(None) => None,
        Err(error) => {
            let base = socket_path_for(session_id).ok()?;
            let control = crate::process::worker::control_socket_sibling(&base);
            let pid = crate::process::worker::peer_pid_from_socket(&control)
                .or_else(|| crate::process::worker::peer_pid_from_socket(&base));
            match pid {
                Some(peer_pid) => warn!(
                    target: "acp.registry",
                    session = %session_id,
                    pid = peer_pid,
                    "worker registry unreadable; recovered runner PID from its socket: {error}"
                ),
                None => warn!(
                    target: "acp.registry",
                    session = %session_id,
                    "worker registry unreadable and no peer PID was available: {error}"
                ),
            }
            pid
        }
    }
}

/// Reap the runner for `session_id`: resolve its PID via `pid_source_for`
/// (on-disk record, or `SO_PEERCRED` on the live socket when the record is
/// unreadable; see #2102), SIGTERM its whole process group, then remove
/// the registry entry and socket.
///
/// The canonical teardown used by the supervisor's shutdown paths and by a
/// fresh spawn that supersedes a stale runner, so no prior agent tree is
/// left orphaned. When a PID is resolved, the signal targets the whole
/// process group (`killpg`) rather than the leader alone: the group can
/// outlive its leader pid, so gating on leader liveness would skip the
/// killpg and leak surviving descendants. `killpg` ignores ESRCH, so an
/// already-empty group is a harmless no-op. See #1689.
pub fn terminate(session_id: &str) {
    let terminated_pid = pid_source_for(session_id);
    if let Some(pid) = terminated_pid {
        crate::process::worker::terminate_process_group(pid);
        delete_if_owned(session_id, pid).ok();
    } else {
        delete_if_absent(session_id).ok();
    }
}
/// Stop a runner and wait through escalation before allowing a replacement to
/// bind its paths. This closes the window where the old process can perform
/// final cleanup after the new process has published its record.
pub async fn terminate_and_wait(session_id: &str) {
    let terminated_pid = pid_source_for(session_id);
    if let Some(pid) = terminated_pid {
        crate::process::worker::terminate_process_group(pid);
        #[cfg(unix)]
        {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            while is_pid_alive(pid) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            crate::process::worker::kill_process_group(pid);
        }
        delete_if_owned(session_id, pid).ok();
    } else {
        delete_if_absent(session_id).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn with_temp_home<F: FnOnce()>(f: F) {
        // Root under /tmp instead of the default $TMPDIR (which on
        // macOS points into /var/folders/... and blows past the
        // ~104-char sun_path limit once we tack on <app_dir>/acp-workers/
        // <session_id>.sock inside a peer_pid test).
        let tmp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let original = std::env::var_os("HOME");
        let original_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: tests are serialized via `#[serial]`; the env mutation
        // window is bounded to this closure and restored on exit.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        f();
        unsafe {
            if let Some(v) = original {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(v) = original_xdg {
                std::env::set_var("XDG_CONFIG_HOME", v);
            } else {
                std::env::remove_var("XDG_CONFIG_HOME");
            }
        }
    }

    #[test]
    #[serial]
    fn roundtrip_save_load() {
        with_temp_home(|| {
            let rec = WorkerRecord::new(
                "sess-abc".into(),
                42,
                PathBuf::from("/tmp/sock"),
                "claude-agent-acp".into(),
                "claude".into(),
                PathBuf::from("/repo"),
                Some("claude-opus-4-7".into()),
                vec![],
                vec!["ANTHROPIC_API_KEY".into()],
                None,
                Some("personal".into()),
            );
            save(&rec).unwrap();
            let loaded = load("sess-abc").unwrap().unwrap();
            assert_eq!(loaded.session_id, "sess-abc");
            assert_eq!(loaded.pid, 42);
            assert_eq!(loaded.runner_version, RUNNER_VERSION);
            assert_eq!(loaded.agent_name, "claude-agent-acp");
            assert_eq!(loaded.agent_key, "claude");
        });
    }

    /// A fresh record is stamped with this binary's build identity and
    /// reports as current; the empty-string legacy default reports stale.
    /// This is the gate the reconciler uses to respawn workers left on an
    /// old binary after `aoe update`. See #1754.
    #[test]
    #[serial]
    fn build_version_stamped_and_current() {
        with_temp_home(|| {
            let rec = WorkerRecord::new(
                "sess-bv".into(),
                1,
                PathBuf::from("/tmp/sess-bv.sock"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            assert_eq!(rec.build_version, crate::build_info::BUILD_VERSION);
            assert!(is_build_current(&rec));

            let mut stale = rec.clone();
            stale.build_version = String::new();
            assert!(
                !is_build_current(&stale),
                "empty (legacy) build_version must read as stale"
            );

            stale.build_version = "0.0.0+gdeadbeef".into();
            assert!(!is_build_current(&stale));
        });
    }

    /// Legacy records written before `build_version` existed must load
    /// with the empty-string default (and thus read as build-stale), not
    /// fail to deserialize.
    #[test]
    #[serial]
    fn load_legacy_record_without_build_version() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            let legacy = serde_json::json!({
                "runner_version": RUNNER_VERSION,
                "session_id": "legacy-bv-1",
                "pid": 7,
                "socket_path": "/tmp/legacy-bv.sock",
                "agent_name": "claude-agent-acp",
                "agent_key": "claude",
                "cwd": "/repo",
                "model": null,
                "additional_dirs": [],
                "provider_env_keys": [],
                "stored_acp_session_id": null,
                "source_profile": null,
                "started_at": 0,
                "last_attached_at": null,
                "detached_at": null
            });
            std::fs::write(
                dir.join("legacy-bv-1.json"),
                serde_json::to_string(&legacy).unwrap(),
            )
            .unwrap();
            let loaded = load("legacy-bv-1").unwrap().unwrap();
            assert_eq!(loaded.build_version, "");
            assert!(!is_build_current(&loaded));
        });
    }

    /// Legacy records written before the `agent_key` field existed
    /// must still load without surfacing a deserialization error;
    /// `serde(default)` fills in the empty string and call sites are
    /// responsible for falling back to `agent_name` or a default
    /// profile. See `Supervisor::agent_key_for_session`.
    #[test]
    #[serial]
    fn load_legacy_record_without_agent_key() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            // Hand-craft a record missing `agent_key` to simulate a
            // file written by an older daemon.
            let legacy = serde_json::json!({
                "runner_version": RUNNER_VERSION,
                "session_id": "legacy-1",
                "pid": 99,
                "socket_path": "/tmp/legacy.sock",
                "agent_name": "claude-agent-acp",
                "cwd": "/repo",
                "model": null,
                "additional_dirs": [],
                "provider_env_keys": [],
                "stored_acp_session_id": null,
                "started_at": 0,
                "last_attached_at": null,
                "detached_at": null
            });
            std::fs::write(
                dir.join("legacy-1.json"),
                serde_json::to_string(&legacy).unwrap(),
            )
            .unwrap();
            let loaded = load("legacy-1").unwrap().unwrap();
            assert_eq!(loaded.agent_name, "claude-agent-acp");
            assert_eq!(loaded.agent_key, "");
        });
    }

    /// Same legacy-record guarantee for `source_profile`: records written
    /// before the field existed must load with `None` (the documented
    /// fallback), not surface a deserialization error.
    #[test]
    #[serial]
    fn load_legacy_record_without_source_profile() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            let legacy = serde_json::json!({
                "runner_version": RUNNER_VERSION,
                "session_id": "legacy-sp-1",
                "pid": 7,
                "socket_path": "/tmp/legacy-sp.sock",
                "agent_name": "claude-agent-acp",
                "agent_key": "claude",
                "cwd": "/repo",
                "model": null,
                "additional_dirs": [],
                "provider_env_keys": [],
                "stored_acp_session_id": null,
                "started_at": 0,
                "last_attached_at": null,
                "detached_at": null
            });
            std::fs::write(
                dir.join("legacy-sp-1.json"),
                serde_json::to_string(&legacy).unwrap(),
            )
            .unwrap();
            let loaded = load("legacy-sp-1").unwrap().unwrap();
            assert_eq!(loaded.source_profile, None);
        });
    }

    /// Fresh records carry `source_profile` end-to-end (write + read).
    /// The roundtrip case is covered above; this asserts the field
    /// specifically because the reattach path depends on it.
    #[test]
    #[serial]
    fn source_profile_roundtrips() {
        with_temp_home(|| {
            let rec = WorkerRecord::new(
                "sess-sp".into(),
                1,
                PathBuf::from("/tmp/sess-sp.sock"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                Some("personal".into()),
            );
            save(&rec).unwrap();
            let loaded = load("sess-sp").unwrap().unwrap();
            assert_eq!(loaded.source_profile.as_deref(), Some("personal"));
        });
    }

    #[test]
    #[serial]
    fn empty_stored_acp_session_id_is_rejected_without_data_loss() {
        with_temp_home(|| {
            let rec = WorkerRecord::new(
                "sess-empty-acp".into(),
                1,
                PathBuf::from("/tmp/sess-empty-acp.sock"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                Some("initial-acp".into()),
                None,
            );
            save(&rec).unwrap();
            let error = update_stored_acp_session_id("sess-empty-acp", 1, "")
                .expect_err("empty session ids are invalid");
            assert!(error.to_string().contains("must not be empty"));
            let loaded = load("sess-empty-acp").unwrap().unwrap();
            assert_eq!(loaded.stored_acp_session_id.as_deref(), Some("initial-acp"));
        });
    }

    #[test]
    #[serial]
    fn list_filters_non_json_and_unparseable() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            std::fs::write(dir.join("not-json.json"), b"this isn't json").unwrap();
            std::fs::write(dir.join("ignored.txt"), b"{}").unwrap();
            let rec = WorkerRecord::new(
                "live".into(),
                1,
                PathBuf::from("/tmp/sock-live"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&rec).unwrap();
            let all = list().unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].session_id, "live");
        });
    }

    #[test]
    #[serial]
    fn delete_removes_json_and_socket() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            let socket = dir.join("sess.sock");
            touch_live_socket(&socket);
            let rec = WorkerRecord::new(
                "sess".into(),
                1,
                socket.clone(),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&rec).unwrap();
            let control = crate::process::worker::control_socket_sibling(&socket);
            assert!(record_path("sess").unwrap().exists());
            assert!(control.exists(), "fixture created the live socket");
            delete("sess").unwrap();
            assert!(!record_path("sess").unwrap().exists());
            assert!(!control.exists(), "delete sweeps the control socket too");
        });
    }

    #[test]
    #[serial]
    fn delete_if_owned_preserves_replacement_record_and_socket() {
        with_temp_home(|| {
            let session_id = "replacement";
            let socket = socket_path_for(session_id).unwrap();
            touch_live_socket(&socket);
            let replacement_control = crate::process::worker::control_socket_sibling(&socket);
            let mut record = WorkerRecord::new(
                session_id.into(),
                111,
                socket,
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&record).unwrap();
            record.pid = 222;
            save(&record).unwrap();

            assert!(!delete_if_owned(session_id, 111).unwrap());
            assert_eq!(load(session_id).unwrap().unwrap().pid, 222);
            assert!(replacement_control.exists());
            assert!(delete_if_owned(session_id, 222).unwrap());
            assert!(load(session_id).unwrap().is_none());
            assert!(!replacement_control.exists());
        });
    }

    #[test]
    #[serial]
    fn delete_sweeps_empty_log_but_keeps_nonempty() {
        with_temp_home(|| {
            let empty_log = log_path_for("empty").unwrap();
            std::fs::create_dir_all(empty_log.parent().unwrap()).unwrap();
            std::fs::write(&empty_log, b"").unwrap();
            delete("empty").unwrap();
            assert!(
                !empty_log.exists(),
                "0-byte worker log should be swept on delete"
            );

            let kept_log = log_path_for("kept").unwrap();
            std::fs::write(&kept_log, b"agent stderr line\n").unwrap();
            delete("kept").unwrap();
            assert!(
                kept_log.exists(),
                "non-empty worker log should survive delete for post-mortem"
            );
        });
    }

    #[test]
    #[serial]
    fn mark_attached_clears_detached() {
        with_temp_home(|| {
            let mut rec = WorkerRecord::new(
                "x".into(),
                1,
                PathBuf::from("/tmp/x.sock"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            rec.detached_at = Some(100);
            save(&rec).unwrap();
            mark_attached("x", 1);
            let after = load("x").unwrap().unwrap();
            assert!(after.last_attached_at.is_some());
            assert!(after.detached_at.is_none());
            let mut replacement = after;
            replacement.pid = 2;
            replacement.detached_at = Some(200);
            save(&replacement).unwrap();
            mark_attached("x", 1);
            let preserved = load("x").unwrap().unwrap();
            assert_eq!(preserved.pid, 2);
            assert_eq!(preserved.detached_at, Some(200));
        });
    }

    #[test]
    #[serial]
    fn terminate_deletes_entry_for_dead_pid() {
        with_temp_home(|| {
            // 2e9 is not a live pid (see is_pid_alive_unlikely_pid), so
            // terminate sends no signal and just clears the stale entry.
            let rec = WorkerRecord::new(
                "term-dead".into(),
                2_000_000_000,
                PathBuf::from("/tmp/term-dead.sock"),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&rec).unwrap();
            assert!(record_path("term-dead").unwrap().exists());
            terminate("term-dead");
            assert!(!record_path("term-dead").unwrap().exists());
        });
    }

    /// The upgrade path #2977 introduced, over a genuinely live process: a
    /// `runner_version: 1` record left by a previous daemon.
    ///
    /// The chain that matters is classification then reaping. A live v1 runner
    /// must read as LIVE (so nothing deletes its record while its PID is the
    /// only copy of where the process is), as NOT runner-current (so the
    /// reconciler replaces it), and `terminate` must actually signal it. Get
    /// the first wrong and the record is dropped with the PID still in it,
    /// which strands the runner and its whole agent subtree with no daemon
    /// able to find them again.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn live_v1_record_is_live_but_stale_and_terminate_reaps_it() {
        use std::os::unix::process::CommandExt as _;

        with_temp_home(|| {
            // Its own process group, so the killpg lands on it alone rather
            // than on the test runner.
            let mut victim = KillOnDrop(
                std::process::Command::new("sleep")
                    .arg("60")
                    .process_group(0)
                    .spawn()
                    .expect("spawn stand-in runner"),
            );
            let dir = workers_dir().unwrap();
            let sock = dir.join("v1sess.sock");
            // A v1 runner bound the relay path itself, not the sibling.
            std::fs::write(&sock, b"").unwrap();
            let mut rec = WorkerRecord::new(
                "v1sess".into(),
                victim.0.id(),
                sock.clone(),
                "aoe-agent".into(),
                "aoe-agent".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            rec.runner_version = 1;
            save(&rec).unwrap();

            assert!(
                is_record_live(&rec),
                "a live v1 runner must not read as dead: its record holds the only copy of the pid"
            );
            assert!(
                !is_runner_current(&rec),
                "v1 is a generation behind, so the reconciler must replace it"
            );

            terminate("v1sess");

            // Signalled, not merely forgotten.
            let reaped = (0..40).any(|_| {
                if matches!(victim.0.try_wait(), Ok(Some(_))) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                false
            });
            assert!(
                reaped,
                "terminate must signal the live v1 runner, not orphan it"
            );
            assert!(!record_path("v1sess").unwrap().exists());
        });
    }

    #[test]
    #[serial]
    fn terminate_missing_entry_is_noop() {
        with_temp_home(|| {
            // No entry, no panic, nothing to delete.
            terminate("does-not-exist");
            assert!(!record_path("does-not-exist").unwrap().exists());
        });
    }

    #[test]
    fn worker_state_ladder() {
        let mut rec = WorkerRecord::new(
            "s".into(),
            1,
            PathBuf::from("/tmp/s.sock"),
            "claude-agent-acp".into(),
            "claude".into(),
            PathBuf::from("/repo"),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        assert_eq!(worker_state_label(&rec, false), "dead");
        assert_eq!(worker_state_label(&rec, true), "attached");
        rec.detached_at = Some(100);
        rec.last_attached_at = Some(50);
        assert_eq!(worker_state_label(&rec, true), "detached");
        rec.last_attached_at = Some(150);
        assert_eq!(worker_state_label(&rec, true), "attached");
        // detached with no prior attach: last_attached_at None is treated as 0,
        // so 0 <= detached_at keeps it detached.
        rec.last_attached_at = None;
        assert_eq!(worker_state_label(&rec, true), "detached");
    }

    #[test]
    fn is_pid_alive_self() {
        let pid = std::process::id();
        assert!(is_pid_alive(pid));
    }

    #[test]
    fn is_pid_alive_unlikely_pid() {
        // PID 0 is the kernel scheduler / swapper; kill(0, 0) targets the
        // *process group*, not a real process. Use a very high value that
        // won't realistically be allocated.
        assert!(!is_pid_alive(2_000_000_000));
    }

    #[test]
    fn validate_session_id_accepts_uuids_and_test_ids() {
        // Production format: UUID v4 with hyphens.
        assert!(
            validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok(),
            "must accept UUID v4 (the production session_id shape)"
        );
        // Test-prefixed ids with underscores and digits.
        assert!(validate_session_id("test_session_42").is_ok());
        assert!(validate_session_id("a").is_ok());
        assert!(validate_session_id("Z-0").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_path_traversal_and_separators() {
        // The whole point of this check: don't let a CLI invocation of
        // `aoe __acp-runner --session-id "<evil>"` write files
        // outside the workers dir.
        for bad in [
            "",
            "..",
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            ".hidden",
            "with space",
            "with\0null",
            "trailing.",
            "good-then/../bad",
        ] {
            assert!(
                validate_session_id(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn validate_session_id_rejects_overlong() {
        let long = "a".repeat(129);
        assert!(validate_session_id(&long).is_err());
        let ok = "a".repeat(128);
        assert!(validate_session_id(&ok).is_ok());
    }

    #[test]
    fn path_builders_propagate_validation_error() {
        // Defense-in-depth: even if some future caller forgets to
        // validate at the trust boundary, the path builders themselves
        // catch a bad id.
        assert!(record_path("../escape").is_err());
        assert!(socket_path_for("foo/bar").is_err());
        assert!(log_path_for("").is_err());
        assert!(restart_marker_path(".hidden").is_err());
    }

    #[test]
    #[serial]
    fn pid_source_for_prefers_record_pid_when_load_ok_some() {
        with_temp_home(|| {
            let rec = WorkerRecord::new(
                "sess-ok-some".into(),
                4242,
                PathBuf::from("/tmp/unused"),
                "claude-agent-acp".into(),
                "claude".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&rec).unwrap();
            assert_eq!(pid_source_for("sess-ok-some"), Some(4242));
        });
    }

    #[test]
    #[serial]
    fn pid_source_for_returns_none_when_load_ok_none() {
        with_temp_home(|| {
            assert_eq!(pid_source_for("sess-missing"), None);
        });
    }

    /// If a corrupt record forces peer probing, current runners expose their
    /// PID on the control socket. The retired raw socket is only a final legacy
    /// fallback.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn pid_source_for_falls_back_to_control_socket_on_load_err() {
        with_temp_home(|| {
            use std::os::unix::fs::PermissionsExt;
            let session_id = "sess-load-err";
            let rec = WorkerRecord::new(
                session_id.into(),
                4242,
                socket_path_for(session_id).unwrap(),
                "claude-agent-acp".into(),
                "claude".into(),
                PathBuf::from("/repo"),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            save(&rec).unwrap();
            let rec_path = record_path(session_id).unwrap();
            std::fs::set_permissions(&rec_path, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(
                load(session_id).is_err(),
                "fixture must force load() to return Err"
            );

            let raw_socket = socket_path_for(session_id).unwrap();
            let control_socket = crate::process::worker::control_socket_sibling(&raw_socket);
            let _listener = std::os::unix::net::UnixListener::bind(&control_socket).unwrap();
            assert_eq!(pid_source_for(session_id), Some(std::process::id()));
        });
    }
}

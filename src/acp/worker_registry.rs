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
//! Layout note: the runner *and* the daemon both write to entries
//! (runner: `pid`/`started_at` on boot; daemon:
//! `last_attached_at`/`detached_at` on attach/detach). We accept the
//! single-writer-per-field convention rather than locking: contention
//! windows are narrow and tearing a single field across an unclean
//! restart at worst causes a re-attach instead of a clean attach.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// Generic worker-subprocess plumbing now lives in `process::worker`; the
// registry is the ACP consumer of it. Re-exported so the names referenced
// across the ACP code (and its tests) keep resolving here.
pub use crate::process::worker::{is_pid_alive, validate_id as validate_session_id};

/// Bump when the on-disk schema changes incompatibly. Older entries with
/// a smaller `runner_version` are swept on startup instead of dialed.
pub const RUNNER_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub runner_version: u32,
    /// Binary build identity (`build_info::BUILD_VERSION`) of the
    /// `aoe __acp-runner` process that wrote this record, e.g.
    /// `"1.9.5+g7f31a9c42e01"`. Distinct from `runner_version`, which is
    /// the on-disk SCHEMA version. The daemon compares this against its
    /// own `BUILD_VERSION` to detect a worker left running on an older
    /// binary after `aoe update` and respawn it (see #1754). Defaulted on
    /// load for legacy records that pre-date this field; the empty string
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Atomic write (temp + rename) with 0600 perms. Avoids the half-written
/// JSON that a naive `fs::write` would leave if the runner is killed
/// mid-write — the dial path would then fail to parse and the entry
/// would be swept.
pub fn save(record: &WorkerRecord) -> Result<()> {
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

/// Remove the JSON entry and the unix socket file (if present). A
/// non-empty `.log` file is intentionally left behind so the user can read
/// it after the worker exits; an empty (0-byte) log carries no post-mortem
/// value and is swept so a crash loop doesn't litter the workers dir with
/// dead empty logs. See #1945.
pub fn delete(session_id: &str) -> Result<()> {
    if let Ok(p) = record_path(session_id) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(p) = socket_path_for(session_id) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(p) = log_path_for(session_id) {
        if matches!(std::fs::metadata(&p), Ok(m) if m.len() == 0) {
            let _ = std::fs::remove_file(&p);
        }
    }
    Ok(())
}

/// Update the `last_attached_at` field in place. Best-effort: any I/O
/// error is logged and swallowed because attach itself has already
/// succeeded; the timestamp is purely for observability.
pub fn mark_attached(session_id: &str) {
    if let Ok(Some(mut rec)) = load(session_id) {
        rec.last_attached_at = Some(now_secs());
        rec.detached_at = None;
        if let Err(e) = save(&rec) {
            debug!(
                target: "acp.registry",
                session = %session_id,
                "failed to update last_attached_at: {e}"
            );
        }
    }
}

pub fn mark_detached(session_id: &str) {
    if let Ok(Some(mut rec)) = load(session_id) {
        rec.detached_at = Some(now_secs());
        if let Err(e) = save(&rec) {
            debug!(
                target: "acp.registry",
                session = %session_id,
                "failed to update detached_at: {e}"
            );
        }
    }
}

/// Update only `stored_acp_session_id` in place. Called by the
/// supervisor when the drain task observes an `AcpSessionAssigned`
/// event, so a fresh `aoe serve` knows to call `session/load` instead
/// of `session/new` on reattach.
pub fn update_stored_acp_session_id(session_id: &str, acp_id: Option<&str>) {
    if let Ok(Some(mut rec)) = load(session_id) {
        rec.stored_acp_session_id = acp_id.filter(|s| !s.is_empty()).map(|s| s.to_string());
        if let Err(e) = save(&rec) {
            debug!(
                target: "acp.registry",
                session = %session_id,
                "failed to update stored_acp_session_id: {e}"
            );
        }
    }
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
    rec.runner_version == RUNNER_VERSION && is_pid_alive(rec.pid) && socket_exists(&rec.socket_path)
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
        Err(e) => {
            let sock = socket_path_for(session_id).ok()?; // same validator as load(); degrades to None on invalid id
            let pid = crate::process::worker::peer_pid_from_socket(&sock);
            match pid {
                Some(peer_pid) => warn!(
                    target: "acp.registry",
                    session = %session_id,
                    pid = peer_pid,
                    "worker registry unreadable; recovered runner PID via SO_PEERCRED on socket: {e}"
                ),
                None => warn!(
                    target: "acp.registry",
                    session = %session_id,
                    "worker registry unreadable and no peer PID on socket; \
                     runner may be orphaned under PID 1: {e}"
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
    if let Some(pid) = pid_source_for(session_id) {
        crate::process::worker::terminate_process_group(pid);
    }
    delete(session_id).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

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
    fn empty_stored_acp_session_id_normalizes_to_none() {
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
            update_stored_acp_session_id("sess-empty-acp", Some(""));
            let loaded = load("sess-empty-acp").unwrap().unwrap();
            assert_eq!(loaded.stored_acp_session_id, None);
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
            let sock = dir.join("sess.sock");
            std::fs::write(&sock, b"").unwrap();
            let rec = WorkerRecord::new(
                "sess".into(),
                1,
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
            save(&rec).unwrap();
            assert!(record_path("sess").unwrap().exists());
            assert!(sock.exists());
            delete("sess").unwrap();
            assert!(!record_path("sess").unwrap().exists());
        });
    }

    #[test]
    #[serial]
    fn delete_sweeps_empty_log_but_keeps_nonempty() {
        with_temp_home(|| {
            // Empty log (worker died before writing anything): swept.
            let empty_log = log_path_for("empty").unwrap();
            std::fs::create_dir_all(empty_log.parent().unwrap()).unwrap();
            std::fs::write(&empty_log, b"").unwrap();
            delete("empty").unwrap();
            assert!(
                !empty_log.exists(),
                "0-byte worker log should be swept on delete"
            );

            // Non-empty log (has post-mortem content): kept.
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
            mark_attached("x");
            let after = load("x").unwrap().unwrap();
            assert!(after.last_attached_at.is_some());
            assert!(after.detached_at.is_none());
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
            // No record on disk AND no socket: the runner is already
            // gone, so we must NOT fall through to the peer probe (the
            // socket, if any, would be a stale inode from a prior spawn).
            assert_eq!(pid_source_for("sess-missing"), None);
        });
    }

    /// #2102: the load-Err branch consults `peer_pid_from_socket` so an
    /// unreadable registry file no longer means the runner
    /// escapes SIGTERM and the shutdown_and_wait poll. Binding a
    /// listener in the test process makes own PID the peer, which is
    /// what the helper must surface.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn pid_source_for_falls_back_to_peer_pid_on_load_err() {
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
            // Force load() -> Err via chmod 0o000.
            let rec_path = record_path(session_id).unwrap();
            std::fs::set_permissions(&rec_path, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(
                load(session_id).is_err(),
                "fixture must force load() to return Err"
            );
            // Bind a real UDS listener in the test process so peer_pid
            // resolves to own PID.
            let sock_path = socket_path_for(session_id).unwrap();
            let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
            assert_eq!(pid_source_for(session_id), Some(std::process::id()));
        });
    }
}

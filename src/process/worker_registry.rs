//! On-disk registry of detached ACP workers at `<app_dir>/acp-workers/<session_id>.json` (mode 0600).
//! Writes and owned deletes serialize on a per-session lock so a superseded runner cannot
//! unlink its replacement's record.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::util::now_secs;

pub use crate::process::worker::{is_pid_alive, validate_id as validate_session_id};

/// Runner protocol generation. Generation 4 announces authoritative native session identity
/// before callbacks; the control wire version stays 3. A stale live process must
/// be authenticated and proven quiescent before its replacement starts.
pub const RUNNER_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub runner_version: u32,
    /// Build identity of the runner that wrote the record; independent of `runner_version`.
    /// Legacy records default to ""; they still require authenticated teardown proof.
    #[serde(default)]
    pub build_version: String,
    pub session_id: String,
    pub pid: u32,
    pub socket_path: PathBuf,
    /// Not the registry key; use `agent_key` to resolve a profile.
    pub agent_name: String,
    #[serde(default)]
    pub agent_key: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub additional_dirs: Vec<PathBuf>,
    pub provider_env_keys: Vec<String>,
    pub stored_acp_session_id: Option<String>,
    #[serde(default)]
    pub source_profile: Option<String>,
    /// Lease ordering for restart markers. Authentication uses the launch nonce.
    #[serde(default)]
    pub generation: u64,
    /// Parent-issued execution ticket; legacy records carry no stop authority.
    #[serde(default)]
    pub launch_nonce: Option<uuid::Uuid>,
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
            generation: 0,
            launch_nonce: None,
            started_at: now_secs(),
            last_attached_at: None,
            detached_at: None,
        }
    }

    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
}

pub fn workers_dir() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?.join("acp-workers");
    crate::process::worker::ensure_dir(&dir)?;
    Ok(dir)
}

pub fn record_path(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::record_path(&workers_dir()?, session_id)
}

pub fn socket_path_for(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::socket_path(&workers_dir()?, session_id)
}

pub fn log_path_for(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::log_path(&workers_dir()?, session_id)
}

/// Written by `aoe acp restart` before the delete and SIGTERM so the daemon treats the
/// teardown as a pending restart. Holds the restarted runner's generation.
pub fn restart_marker_path(session_id: &str) -> Result<PathBuf> {
    crate::process::worker::restart_marker_path(&workers_dir()?, session_id)
}

pub fn mark_restart_pending(session_id: &str, generation: u64) {
    let Ok(path) = restart_marker_path(session_id) else {
        return;
    };
    // Published by rename so a claim can never see a half-written marker.
    let staged = path.with_extension(format!("restart.tmp-{}", std::process::id()));
    if std::fs::write(&staged, generation.to_string()).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600));
    }
    if std::fs::rename(&staged, &path).is_err() {
        let _ = std::fs::remove_file(&staged);
    }
}

pub fn peek_restart_marker(session_id: &str) -> Option<u64> {
    let path = restart_marker_path(session_id).ok()?;
    crate::process::worker::read_restart_marker(&path)
}

/// Renamed aside before reading, so a marker for a newer runner written in between survives.
/// `None` when absent; `Some(None)` when it names no generation.
pub fn claim_restart_marker(session_id: &str) -> Option<Option<u64>> {
    let path = restart_marker_path(session_id).ok()?;
    let claim = path.with_extension(format!("restart.claim-{}", std::process::id()));
    std::fs::rename(&path, &claim).ok()?;
    let generation = crate::process::worker::read_restart_marker(&claim);
    let _ = std::fs::remove_file(&claim);
    Some(generation)
}

/// A zero marker matches only a zero identity. The file is removed either way.
pub fn take_restart_marker(session_id: &str, generation: u64) -> bool {
    claim_restart_marker(session_id).flatten() == Some(generation)
}

pub fn clear_restart_marker(session_id: &str) {
    if let Ok(path) = restart_marker_path(session_id) {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn save(record: &WorkerRecord) -> Result<()> {
    with_registry_lock(&record.session_id, || save_unlocked(record))
}

/// Publishes the listener and record under the same fence as owned cleanup.
#[cfg(unix)]
pub(crate) fn publish_control_listener(
    record: &WorkerRecord,
    socket: &Path,
) -> Result<tokio::net::UnixListener> {
    with_registry_lock(&record.session_id, || {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_file(socket);
        let listener = tokio::net::UnixListener::bind(socket)
            .with_context(|| format!("binding {}", socket.display()))?;
        let publish = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .context("securing runner control socket")
            .and_then(|()| save_unlocked(record));
        if let Err(error) = publish {
            let _ = std::fs::remove_file(socket);
            return Err(error);
        }
        Ok(listener)
    })
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
/// [`load`] without folding a record it cannot read into "no runner": an
/// unreadable record answers `Err`, which a caller about to destroy a checkout
/// has to refuse on rather than treat as an absent session.
pub fn load_strict(session_id: &str) -> Result<Option<WorkerRecord>> {
    load_strict_unlocked(session_id)
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

#[cfg(test)]
pub(crate) fn delete(session_id: &str) -> Result<()> {
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

fn update_if_owned(
    session_id: &str,
    owner_pid: u32,
    owner_generation: u64,
    owner_nonce: uuid::Uuid,
    update: impl FnOnce(&mut WorkerRecord),
) -> Result<bool> {
    with_registry_lock(session_id, || {
        let Some(mut record) = load_strict_unlocked(session_id)? else {
            return Ok(false);
        };
        if record.pid != owner_pid
            || record.generation != owner_generation
            || record.launch_nonce != Some(owner_nonce)
        {
            return Ok(false);
        }
        update(&mut record);
        save_unlocked(&record)?;
        Ok(true)
    })
}

pub fn mark_attached(session_id: &str, owner_pid: u32, generation: u64, nonce: uuid::Uuid) {
    if let Err(error) = update_if_owned(session_id, owner_pid, generation, nonce, |record| {
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

pub fn mark_detached(session_id: &str, owner_pid: u32, generation: u64, nonce: uuid::Uuid) {
    if let Err(error) = update_if_owned(session_id, owner_pid, generation, nonce, |record| {
        record.detached_at = Some(now_secs());
    }) {
        debug!(
            target: "acp.registry",
            session = %session_id,
            "failed to update detached_at: {error}"
        );
    }
}

pub fn update_stored_acp_session_id(
    session_id: &str,
    owner_pid: u32,
    generation: u64,
    nonce: uuid::Uuid,
    acp_id: &str,
) -> Result<()> {
    anyhow::ensure!(!acp_id.is_empty(), "ACP session id must not be empty");
    let updated = update_if_owned(session_id, owner_pid, generation, nonce, |record| {
        record.stored_acp_session_id = Some(acp_id.to_string());
    })?;
    anyhow::ensure!(updated, "runner no longer owns its registry record");
    Ok(())
}

/// Live requires both a live pid and the socket file, which guards against pid reuse.
pub fn is_record_live(rec: &WorkerRecord) -> bool {
    is_pid_alive(rec.pid) && socket_exists(&expected_socket(rec))
}

fn expected_socket(rec: &WorkerRecord) -> std::path::PathBuf {
    if rec.runner_version >= 2 {
        crate::process::worker::control_socket_sibling(&rec.socket_path)
    } else {
        rec.socket_path.clone()
    }
}

pub(crate) fn remove_runner_sockets(socket_path: &Path) {
    for path in [
        socket_path.to_path_buf(),
        crate::process::worker::control_socket_sibling(socket_path),
    ] {
        let _ = std::fs::remove_file(&path);
    }
}

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

pub fn is_runner_current(rec: &WorkerRecord) -> bool {
    rec.runner_version == RUNNER_VERSION
}

/// Not folded into `is_record_live`: a busy stale worker must not look dead.
pub fn is_build_current(rec: &WorkerRecord) -> bool {
    rec.build_version == crate::build_info::BUILD_VERSION
}

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

pub fn delete_if_owned_by(
    session_id: &str,
    pid: u32,
    generation: u64,
    launch_nonce: Option<uuid::Uuid>,
) -> bool {
    with_registry_lock(session_id, || match load_strict_unlocked(session_id)? {
        Some(rec)
            if rec.pid != pid
                || rec.generation != generation
                || rec.launch_nonce != launch_nonce =>
        {
            debug!(
                target: "acp.registry",
                session = %session_id,
                current_pid = rec.pid,
                "leaving registry entry; it belongs to a replacement runner"
            );
            Ok(true)
        }
        _ => delete_unlocked(session_id).map(|()| true),
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_temp_home<F: FnOnce()>(f: F) {
        // Keep worker socket paths below macOS sun_path limits.
        let tmp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        f();
    }

    /// A minimal record; tests that care about other fields set them on the result.
    fn new_record(session_id: &str, pid: u32, socket: impl Into<PathBuf>) -> WorkerRecord {
        WorkerRecord::new(
            session_id.into(),
            pid,
            socket.into(),
            "aoe-agent".into(),
            "aoe-agent".into(),
            PathBuf::from("/repo"),
            None,
            vec![],
            vec![],
            None,
            None,
        )
    }

    #[test]
    #[serial]
    fn roundtrip_save_load() {
        with_temp_home(|| {
            let mut rec = new_record("sess-abc", 42, "/tmp/sock");
            rec.agent_name = "claude-agent-acp".into();
            rec.agent_key = "claude".into();
            rec.model = Some("claude-opus-4-7".into());
            rec.provider_env_keys = vec!["ANTHROPIC_API_KEY".into()];
            rec.source_profile = Some("personal".into());
            save(&rec).unwrap();
            let loaded = load("sess-abc").unwrap().unwrap();
            assert_eq!(loaded.session_id, "sess-abc");
            assert_eq!(loaded.pid, 42);
            assert_eq!(loaded.runner_version, RUNNER_VERSION);
            assert_eq!(loaded.agent_name, "claude-agent-acp");
            assert_eq!(loaded.agent_key, "claude");
            assert_eq!(loaded.source_profile.as_deref(), Some("personal"));
            assert_eq!(loaded.build_version, crate::build_info::BUILD_VERSION);
            assert!(is_build_current(&loaded));

            let mut stale = loaded;
            stale.build_version = String::new();
            assert!(
                !is_build_current(&stale),
                "empty (legacy) build_version must read as stale"
            );

            stale.build_version = "0.0.0+gdeadbeef".into();
            assert!(!is_build_current(&stale));
        });
    }

    #[test]
    #[serial]
    fn load_fills_defaults_for_fields_legacy_records_lack() {
        with_temp_home(|| {
            let cases: [(&str, &[&str], fn(&WorkerRecord)); 3] = [
                ("legacy-bv", &["build_version"], |rec| {
                    assert_eq!(rec.build_version, "");
                    assert!(!is_build_current(rec));
                }),
                ("legacy-ak", &["agent_key"], |rec| {
                    assert_eq!(rec.agent_name, "claude-agent-acp");
                    assert_eq!(rec.agent_key, "");
                }),
                ("legacy-sp", &["source_profile"], |rec| {
                    assert_eq!(rec.source_profile, None);
                }),
            ];
            for (session_id, dropped, check) in cases {
                let mut legacy = serde_json::json!({
                    "runner_version": RUNNER_VERSION,
                    "build_version": crate::build_info::BUILD_VERSION,
                    "session_id": session_id,
                    "pid": 7,
                    "socket_path": format!("/tmp/{session_id}.sock"),
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
                for field in dropped {
                    legacy.as_object_mut().unwrap().remove(*field);
                }
                let path = workers_dir().unwrap().join(format!("{session_id}.json"));
                std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
                check(&load(session_id).unwrap().unwrap());
            }
        });
    }

    #[test]
    #[serial]
    fn empty_stored_acp_session_id_is_rejected_without_data_loss() {
        with_temp_home(|| {
            let mut rec = new_record("sess-empty-acp", 1, "/tmp/sess-empty-acp.sock");
            rec.stored_acp_session_id = Some("initial-acp".into());
            save(&rec).unwrap();
            assert!(update_stored_acp_session_id(
                "sess-empty-acp",
                1,
                0,
                uuid::Uuid::from_u128(1),
                ""
            )
            .is_err());
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
            let rec = new_record("live", 1, "/tmp/sock-live");
            save(&rec).unwrap();
            let all = list().unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].session_id, "live");
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
            let mut record = new_record(session_id, 111, socket);
            save(&record).unwrap();
            record.pid = 222;
            save(&record).unwrap();

            assert!(delete_if_owned_by(session_id, 111, 0, None));
            assert_eq!(load(session_id).unwrap().unwrap().pid, 222);
            assert!(replacement_control.exists());
            assert!(delete_if_owned_by(session_id, 222, 0, None));
            assert!(load(session_id).unwrap().is_none());
            assert!(!replacement_control.exists());
        });
    }

    #[test]
    #[serial]
    fn mark_attached_clears_detached() {
        with_temp_home(|| {
            let mut rec = new_record("x", 1, "/tmp/x.sock");
            let nonce = uuid::Uuid::from_u128(1);
            rec.launch_nonce = Some(nonce);
            rec.detached_at = Some(100);
            save(&rec).unwrap();
            mark_attached("x", 1, 0, nonce);
            let after = load("x").unwrap().unwrap();
            assert!(after.last_attached_at.is_some());
            assert!(after.detached_at.is_none());
            let mut replacement = after;
            replacement.launch_nonce = Some(uuid::Uuid::from_u128(2));
            replacement.stored_acp_session_id = Some("replacement-session".into());
            replacement.detached_at = Some(200);
            save(&replacement).unwrap();
            mark_attached("x", 1, 0, nonce);
            assert!(update_stored_acp_session_id("x", 1, 0, nonce, "obsolete-session").is_err());
            let preserved = load("x").unwrap().unwrap();
            assert_eq!(preserved.launch_nonce, Some(uuid::Uuid::from_u128(2)));
            assert_eq!(preserved.detached_at, Some(200));
            assert_eq!(
                preserved.stored_acp_session_id.as_deref(),
                Some("replacement-session")
            );
        });
    }

    #[test]
    #[serial]
    fn delete_removes_json_and_socket() {
        with_temp_home(|| {
            let dir = workers_dir().unwrap();
            let socket = dir.join("sess.sock");
            touch_live_socket(&socket);
            let rec = new_record("sess", 1, socket.clone());
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
    fn worker_state_ladder() {
        let mut rec = new_record("s", 1, "/tmp/s.sock");
        assert_eq!(worker_state_label(&rec, false), "dead");
        assert_eq!(worker_state_label(&rec, true), "attached");
        rec.detached_at = Some(100);
        rec.last_attached_at = Some(50);
        assert_eq!(worker_state_label(&rec, true), "detached");
        rec.last_attached_at = Some(150);
        assert_eq!(worker_state_label(&rec, true), "attached");
        rec.last_attached_at = None;
        assert_eq!(worker_state_label(&rec, true), "detached");
    }

    #[test]
    fn path_builders_propagate_validation_error() {
        assert!(record_path("../escape").is_err());
        assert!(socket_path_for("foo/bar").is_err());
        assert!(log_path_for("").is_err());
        assert!(restart_marker_path(".hidden").is_err());
    }

    #[test]
    #[serial]
    fn restart_marker_authority_is_bound_to_a_generation() {
        with_temp_home(|| {
            assert!(!take_restart_marker("m", 7), "no marker, nothing to take");
            mark_restart_pending("m", 7);
            assert_eq!(peek_restart_marker("m"), Some(7));
            assert!(
                !take_restart_marker("m", 8),
                "a marker for another generation grants nothing"
            );
            assert_eq!(peek_restart_marker("m"), None, "but it is still consumed");

            mark_restart_pending("m", 7);
            assert!(take_restart_marker("m", 7));
            assert_eq!(peek_restart_marker("m"), None);

            mark_restart_pending("m", 0);
            assert!(
                !take_restart_marker("m", 7),
                "a legacy marker grants nothing to a generation it did not name"
            );
            mark_restart_pending("m", 0);
            assert!(
                take_restart_marker("m", 0),
                "a legacy marker restarts a legacy (pre-generation) runner"
            );

            let path = restart_marker_path("m").unwrap();
            std::fs::write(&path, b"").unwrap();
            assert_eq!(
                peek_restart_marker("m"),
                None,
                "an empty legacy marker is unbound"
            );
            assert_eq!(
                claim_restart_marker("m"),
                Some(None),
                "a malformed marker is claimed and reported unbound"
            );
            assert!(!path.exists(), "a claim leaves nothing behind");
            assert_eq!(claim_restart_marker("m"), None, "nothing left to claim");

            mark_restart_pending("m", 8);
            let claimed = claim_restart_marker("m");
            mark_restart_pending("m", 9);
            assert_eq!(claimed, Some(Some(8)));
            assert_eq!(peek_restart_marker("m"), Some(9));
            let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("restart."))
                .collect();
            assert!(
                leftovers.is_empty(),
                "publication leaves no staging file: {leftovers:?}"
            );
            clear_restart_marker("m");
            assert!(!path.exists());
        });
    }

    #[test]
    #[serial]
    fn delete_if_owned_by_leaves_a_replacement_record() {
        with_temp_home(|| {
            let socket = workers_dir().unwrap().join("g.sock");
            let nonce = uuid::Uuid::from_u128(1);
            let replacement = uuid::Uuid::from_u128(2);
            let mut rec = new_record("g", 41, socket).with_generation(3);
            rec.launch_nonce = Some(nonce);
            save(&rec).unwrap();
            assert!(
                delete_if_owned_by("g", 40, 3, Some(nonce)),
                "other pid: settled without touching"
            );
            assert!(load("g").unwrap().is_some());
            assert!(
                delete_if_owned_by("g", 41, 4, Some(nonce)),
                "other generation: settled, kept"
            );
            assert!(load("g").unwrap().is_some());
            assert!(delete_if_owned_by("g", 41, 3, Some(replacement)));
            assert_eq!(load("g").unwrap().unwrap().launch_nonce, Some(nonce));
            assert!(delete_if_owned_by("g", 41, 3, None));
            assert_eq!(load("g").unwrap().unwrap().launch_nonce, Some(nonce));
            assert!(delete_if_owned_by("g", 41, 3, Some(nonce)));
            assert!(load("g").unwrap().is_none());
            assert!(
                delete_if_owned_by("g", 41, 3, Some(nonce)),
                "missing record is settled"
            );
        });
    }
}

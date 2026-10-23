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

/// Generation 7 binds runner retirement to a kernel birth identity.
pub const RUNNER_VERSION: u32 = 7;

/// Frozen scope authority for this runner, never a session-wide wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScopeOwnership {
    Unscoped,
    Owned { unit: String },
    Unresolved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub runner_version: u32,
    /// Build identity of the runner that wrote the record; independent of `runner_version`.
    /// Legacy records default to "", which forces a one-time respawn.
    #[serde(default)]
    pub build_version: String,
    pub session_id: String,
    pub pid: u32,
    #[serde(default)]
    pub runner_identity: Option<crate::process::ProcessBirthIdentity>,
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
    #[serde(default)]
    pub prime_runtime: Option<crate::process::prime::PrimeRuntime>,
    /// Together with `pid`, identifies the exact runner process. Older records default to 0.
    #[serde(default)]
    pub generation: u64,
    pub scope: ScopeOwnership,
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
            runner_identity: crate::process::process_birth_identity(pid).ok().flatten(),
            socket_path,
            agent_name,
            agent_key,
            cwd,
            model,
            additional_dirs,
            provider_env_keys,
            stored_acp_session_id,
            source_profile,
            prime_runtime: None,
            generation: 0,
            scope: ScopeOwnership::Unscoped,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAuthority {
    Automatic,
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StopIntent {
    pid: u32,
    generation: u64,
    token: String,
}

fn stop_path(session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    Ok(workers_dir()?.join(format!("{session_id}.stop")))
}

pub(crate) fn stop_intent(session_id: &str) -> Result<Option<StopIntent>> {
    match std::fs::read(stop_path(session_id)?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("reading worker stop intent")
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading worker stop intent"),
    }
}

pub(crate) fn stop_requested(session_id: &str) -> bool {
    match stop_intent(session_id) {
        Ok(intent) => intent.is_some(),
        Err(error) => {
            warn!(target: "acp.registry", %session_id, %error, "unreadable stop intent retains admission fence");
            true
        }
    }
}

fn same_runner(current: &WorkerRecord, selected: &WorkerRecord) -> bool {
    current.pid == selected.pid
        && current.generation == selected.generation
        && current.runner_identity == selected.runner_identity
        && current.started_at == selected.started_at
}

pub(crate) fn request_stop(selected: &WorkerRecord) -> Result<StopIntent> {
    with_registry_lock(&selected.session_id, || {
        let current = load_strict_unlocked(&selected.session_id)?
            .context("worker disappeared before stop admission")?;
        anyhow::ensure!(
            same_runner(&current, selected),
            "worker changed before stop admission"
        );
        let intent = StopIntent {
            pid: selected.pid,
            generation: selected.generation,
            token: uuid::Uuid::new_v4().simple().to_string(),
        };
        crate::session::atomic_write(
            &stop_path(&selected.session_id)?,
            &serde_json::to_vec(&intent)?,
        )?;
        Ok(intent)
    })
}

pub(crate) fn admit_resume(
    session_id: &str,
    authority: ResumeAuthority,
    observed: &Option<StopIntent>,
) -> Result<()> {
    with_registry_lock(session_id, || {
        let current = stop_intent(session_id)?;
        let record = load_strict_unlocked(session_id)?;
        if current.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            authority == ResumeAuthority::Explicit && &current == observed,
            "worker stop intent fences automatic or superseded resume"
        );
        anyhow::ensure!(
            record.is_none(),
            "worker shutdown is not settled; resume remains fenced"
        );
        std::fs::remove_file(stop_path(session_id)?)?;
        std::fs::File::open(workers_dir()?)?.sync_all()?;
        Ok(())
    })
}

pub(crate) fn retire_selected(selected: &WorkerRecord) -> Result<()> {
    with_registry_lock(&selected.session_id, || {
        if let Some(current) = load_strict_unlocked(&selected.session_id)? {
            anyhow::ensure!(
                same_runner(&current, selected),
                "worker changed during shutdown; replacement was not retired"
            );
        }
        delete_unlocked(&selected.session_id)
    })
}

pub(crate) fn register_runner(record: &WorkerRecord) -> Result<tokio::net::UnixListener> {
    with_registry_lock(&record.session_id, || {
        anyhow::ensure!(
            stop_intent(&record.session_id)?.is_none(),
            "worker launch cancelled by a stop request"
        );
        anyhow::ensure!(
            load_strict_unlocked(&record.session_id)?.is_none(),
            "previous worker ownership remains"
        );
        anyhow::ensure!(
            record.runner_identity.is_some(),
            "new runner birth identity unavailable"
        );
        let control = crate::process::worker::control_socket_sibling(&record.socket_path);
        anyhow::ensure!(
            crate::process::worker::peer_pid_from_socket(&control).is_none(),
            "worker control socket still has a live owner"
        );
        if control.try_exists()? {
            std::fs::remove_file(&control)?;
        }
        let listener = tokio::net::UnixListener::bind(&control)?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o600))?;
        if let Err(error) = save_unlocked(record) {
            let _ = std::fs::remove_file(&control);
            return Err(error);
        }
        Ok(listener)
    })
}

pub(crate) fn recover_worker(session_id: &str) -> Result<()> {
    let path = record_path(session_id)?;
    let selected = std::fs::read(&path)?;
    let record: Option<WorkerRecord> = serde_json::from_slice(&selected).ok();
    if let Some(record) = &record {
        if let Some(expected) = &record.runner_identity {
            let runner = crate::process::RunnerProcessGuard::capture(
                record.pid,
                Some(expected.fingerprint()),
            )?;
            anyhow::ensure!(
                !runner.executing()?,
                "original runner or its group is still executing; recovery refused"
            );
        } else {
            anyhow::ensure!(
                record.runner_version < RUNNER_VERSION || !is_pid_alive(record.pid),
                "original runner is still alive or unattested; recovery refused"
            );
            anyhow::ensure!(
                !crate::process::runner_group_executing(record.pid)?,
                "runner group still has executing processes; stop the original execution externally before recovery"
            );
        }
        if record
            .prime_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.proof_path.is_some())
        {
            cleanup_runtime(record)?;
        }
    }
    with_registry_lock(session_id, || {
        anyhow::ensure!(
            std::fs::read(&path)? == selected,
            "worker ownership changed during recovery"
        );
        let control = crate::process::worker::control_socket_sibling(&socket_path_for(session_id)?);
        anyhow::ensure!(
            crate::process::worker::peer_pid_from_socket(&control).is_none(),
            "worker control socket still has a live owner; recovery refused"
        );
        retire_record_files(session_id)
    })
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

fn save_unlocked(record: &WorkerRecord) -> Result<()> {
    let current = load_strict_unlocked(&record.session_id)?;
    anyhow::ensure!(
        stop_intent(&record.session_id)?.is_none()
            || current
                .as_ref()
                .is_some_and(|current| same_runner(current, record)),
        "worker publication cancelled by a stop request"
    );
    let path = record_path(&record.session_id)?;
    crate::session::atomic_write(&path, &serde_json::to_vec_pretty(record)?)
        .context("publishing worker record")
}

pub(crate) fn acquire_registry_lock(session_id: &str) -> Result<std::fs::File> {
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
    Ok(lock_file)
}

pub(crate) fn with_registry_lock<T>(
    session_id: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock_file = acquire_registry_lock(session_id)?;
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

/// Locked, strict read for destructive ownership decisions. Unlike [`load`],
/// malformed or unreadable state remains an error instead of looking absent.
pub(crate) fn load_strict(session_id: &str) -> Result<Option<WorkerRecord>> {
    with_registry_lock(session_id, || load_strict_unlocked(session_id))
}
fn load_strict_unlocked(session_id: &str) -> Result<Option<WorkerRecord>> {
    let path = record_path(session_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Unreadable worker ownership at {}. Stop the original runner and all detached native processes externally, then run aoe acp recover {session_id} --confirm-worker-stopped; evidence is retained", path.display()))
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

pub fn delete(session_id: &str) -> Result<()> {
    with_registry_lock(session_id, || delete_unlocked(session_id))
}

fn delete_unlocked(session_id: &str) -> Result<()> {
    load_strict_unlocked(session_id)?;
    retire_record_files(session_id)
}

fn retire_record_files(session_id: &str) -> Result<()> {
    remove_runner_sockets(&socket_path_for(session_id)?)?;
    let path = record_path(session_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("retiring worker ownership record"),
    }
    std::fs::File::open(workers_dir()?)?.sync_all()?;
    let log = log_path_for(session_id)?;
    if matches!(std::fs::metadata(&log), Ok(metadata) if metadata.len() == 0) {
        let _ = std::fs::remove_file(log);
    }
    Ok(())
}

/// A replacement save uses the same lock, so it lands either before the check or after cleanup.
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

pub fn update_stored_acp_session_id(session_id: &str, owner_pid: u32, acp_id: &str) -> Result<()> {
    anyhow::ensure!(!acp_id.is_empty(), "ACP session id must not be empty");
    let updated = update_if_owned(session_id, owner_pid, |record| {
        record.stored_acp_session_id = Some(acp_id.to_string());
    })?;
    anyhow::ensure!(updated, "runner no longer owns its registry record");
    Ok(())
}

/// A live registry owner has the recorded birth and its expected socket.
pub fn is_record_live(rec: &WorkerRecord) -> bool {
    let same_birth = rec.runner_identity.as_ref().is_none_or(|expected| {
        crate::process::process_birth_identity(rec.pid)
            .map(|actual| actual.as_ref() == Some(expected))
            .unwrap_or(false)
    });
    same_birth && is_pid_alive(rec.pid) && socket_exists(&expected_socket(rec))
}

fn expected_socket(rec: &WorkerRecord) -> std::path::PathBuf {
    if rec.runner_version >= 2 {
        crate::process::worker::control_socket_sibling(&rec.socket_path)
    } else {
        rec.socket_path.clone()
    }
}

pub(crate) fn remove_runner_sockets(socket_path: &Path) -> Result<()> {
    for path in [
        socket_path.to_path_buf(),
        crate::process::worker::control_socket_sibling(socket_path),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("retiring worker socket {}", path.display()))
            }
        }
    }
    Ok(())
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

/// Falls back to `SO_PEERCRED` only when `load` errors; `Ok(None)` means the runner is gone.
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

/// Settles a selected runner and its native runtime before retiring ownership.
pub fn terminate(session_id: &str) -> Result<()> {
    if let Some(record) = load(session_id)? {
        settle_runner_sync(&record, std::time::Duration::from_secs(2))?;
        cleanup_runtime(&record)?;
        retire_selected(&record)
    } else {
        delete_if_absent(session_id).map(|_| ())
    }
}

fn settle_runner_sync(record: &WorkerRecord, grace: std::time::Duration) -> Result<()> {
    let fingerprint = if let Some(expected) = &record.runner_identity {
        let current_boot = crate::process::boot_id()
            .ok_or_else(|| anyhow::anyhow!("current boot identity unavailable"))?;
        if current_boot != expected.boot_id {
            return Ok(());
        }
        Some(expected.fingerprint())
    } else {
        let before = crate::process::runner_birth_identity(record.pid)?;
        if before.is_some() {
            anyhow::ensure!(crate::process::worker::peer_pid_from_socket(&expected_socket(record)) == Some(record.pid),
                "Legacy runner identity is unattested; use aoe acp recover {} --confirm-worker-stopped after external settlement", record.session_id);
            anyhow::ensure!(
                crate::process::runner_birth_identity(record.pid)? == before,
                "runner incarnation changed while attesting its control socket"
            );
        }
        before.map(|identity| identity.fingerprint())
    };
    crate::process::RunnerProcessGuard::capture(record.pid, fingerprint)
        .and_then(|runner| runner.settle(grace))
        .with_context(|| format!("runner termination unproven; stop the original execution externally, then run aoe acp recover {} --confirm-worker-stopped", record.session_id))
}

pub(crate) async fn settle_runner(record: &WorkerRecord, grace: std::time::Duration) -> Result<()> {
    let record = record.clone();
    tokio::task::spawn_blocking(move || settle_runner_sync(&record, grace)).await?
}

/// Settles the selected generation before replacement.
pub async fn terminate_and_wait(session_id: &str) -> anyhow::Result<()> {
    if let Some(record) = load(session_id)? {
        terminate_record_and_wait(record, ()).await
    } else {
        delete_if_absent(session_id)?;
        Ok(())
    }
}

/// Holds the cleanup claim in both the async owner and any uncancellable blocking operation.
pub(crate) async fn terminate_record_and_wait(
    record: WorkerRecord,
    cleanup_guard: impl Clone + Send + 'static,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        load(&record.session_id)?.is_some_and(|current| same_runner(&current, &record)),
        "worker generation changed before termination"
    );
    let blocking_guard = cleanup_guard.clone();
    let record = tokio::task::spawn_blocking(move || {
        let _guard = blocking_guard;
        settle_runner_sync(&record, std::time::Duration::from_secs(2))?;
        cleanup_runtime(&record)?;
        Ok::<_, anyhow::Error>(record)
    })
    .await??;
    retire_selected(&record)?;
    drop(cleanup_guard);
    Ok(())
}

pub(crate) fn cleanup_runtime(record: &WorkerRecord) -> anyhow::Result<()> {
    anyhow::ensure!(
        !(record.agent_key == "prime-agent" || record.prime_runtime.is_some())
            || record.prime_runtime.as_ref().is_some_and(|runtime| runtime.proof_path.is_some()),
        "Prime worker has no generation-owned native route. Stop the original execution's native processes externally and wait for its runner to exit, then run aoe acp recover {} --confirm-prime-stopped; AoE cannot verify that shutdown.",
        record.session_id
    );
    if let Some(runtime) = &record.prime_runtime {
        runtime.cleanup()?;
    }
    Ok(())
}

/// Retires only missing-route Prime evidence after the caller confirms external native shutdown.
pub(crate) fn recover_untracked_prime(selected: &WorkerRecord) -> Result<()> {
    with_registry_lock(&selected.session_id, || {
        let current = load_strict_unlocked(&selected.session_id)?
            .ok_or_else(|| anyhow::anyhow!("worker record disappeared before recovery"))?;
        anyhow::ensure!(
            same_runner(&current, selected),
            "worker generation changed before recovery"
        );
        anyhow::ensure!(
            (current.agent_key == "prime-agent" || current.prime_runtime.is_some())
                && current
                    .prime_runtime
                    .as_ref()
                    .is_none_or(|runtime| runtime.proof_path.is_none()),
            "recovery is only for Prime records without a native ownership route"
        );
        anyhow::ensure!(
            current.runner_identity.as_ref().map_or_else(
                || Ok(!is_pid_alive(current.pid)),
                |expected| crate::process::runner_birth_identity(current.pid)
                    .map(|actual| actual.as_ref() != Some(expected))
            )?,
            "runner is still alive; recovery refused"
        );
        delete_unlocked(&selected.session_id)
    })
}

pub fn delete_if_owned_by(
    session_id: &str,
    identity: crate::acp::runner_lifecycle::RunnerIdentity,
) -> bool {
    with_registry_lock(session_id, || match load_strict_unlocked(session_id)? {
        Some(rec) if identity.disowns_record(&rec) => {
            debug!(
                target: "acp.registry",
                session = %session_id,
                current_pid = rec.pid,
                "leaving registry entry; it belongs to a replacement runner"
            );
            Ok(true)
        }
        Some(rec) if !identity.owns_record(&rec) => Ok(false),
        _ => delete_unlocked(session_id).map(|()| true),
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    #[cfg(target_os = "linux")]
    #[serial]
    fn cold_orphan_recovery_requires_external_group_settlement_and_preserves_live_owners() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        let root = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let mut leader = KillOnDrop(
            std::process::Command::new("bash")
                .args(["-c", "sleep 600 & printf '%s\\n' \"$!\"; read -r ignored"])
                .process_group(0)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut line = String::new();
        std::io::BufReader::new(leader.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let member_pid = line.trim().parse::<u32>().unwrap();
        let member = crate::process::PrimeProcessIdentity {
            pid: member_pid,
            start_id: crate::process::process_birth_identity(member_pid)
                .unwrap()
                .unwrap()
                .start_id,
        };
        struct Member(crate::process::PrimeProcessIdentity);
        impl Drop for Member {
            fn drop(&mut self) {
                let _ = crate::process::fence_processes(
                    std::slice::from_ref(&self.0),
                    std::time::Duration::ZERO,
                );
            }
        }
        let member = Member(member);
        let record = new_record(
            "cold-orphan",
            leader.0.id(),
            socket_path_for("cold-orphan").unwrap(),
        );
        save(&record).unwrap();
        let path = record_path(&record.session_id).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(recover_worker(&record.session_id).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        drop(leader.0.stdin.take());
        leader.0.wait().unwrap();
        assert!(crate::process::runner_group_executing(record.pid).unwrap());
        assert!(terminate(&record.session_id).is_err());
        assert!(recover_worker(&record.session_id).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        crate::process::fence_processes(std::slice::from_ref(&member.0), std::time::Duration::ZERO)
            .unwrap();
        assert!(!crate::process::runner_group_executing(record.pid).unwrap());
        recover_worker(&record.session_id).unwrap();
        assert!(load(&record.session_id).unwrap().is_none());
    }

    #[tokio::test]
    #[serial]
    async fn stale_birth_never_signals_a_reused_runner_pid() {
        use crate::acp::runner_lifecycle::{ProcessControl, RunnerIdentity, SystemProcessControl};
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::process::CommandExt as _;
        let root = TempDir::with_prefix_in("aoe-birth-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let mut child = KillOnDrop(
            std::process::Command::new("cat")
                .process_group(0)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        child
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"ready\n")
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let mut stale = new_record("reused", child.0.id(), socket_path_for("reused").unwrap());
        stale.runner_identity.as_mut().unwrap().start_id = "obsolete-birth".into();
        save(&stale).unwrap();
        let identity = RunnerIdentity::from_record(&stale);
        let control = SystemProcessControl::default();
        control.terminate_group(identity);
        control.kill_group(identity);
        assert!(!control.is_alive(identity));
        settle_runner(&stale, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(!is_record_live(&stale));
        terminate_and_wait("reused").await.unwrap();
        assert!(load("reused").unwrap().is_none());
        assert!(child.0.try_wait().unwrap().is_none());
        control.forget(identity);
    }

    #[tokio::test]
    #[serial]
    async fn durable_stop_fences_publication_and_only_latest_explicit_resume_clears_it() {
        let root = TempDir::with_prefix_in("aoe-stop-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let id = "stop-cas";
        let socket = socket_path_for(id).unwrap();
        let record = new_record(id, std::process::id(), socket.clone()).with_generation(1);
        save(&record).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
        let first = request_stop(&record).unwrap();
        assert!(admit_resume(id, ResumeAuthority::Automatic, &Some(first.clone())).is_err());
        assert!(admit_resume(id, ResumeAuthority::Explicit, &Some(first.clone())).is_err());
        let replacement = record.clone().with_generation(2);
        assert!(register_runner(&replacement).is_err());
        assert_eq!(
            crate::process::worker::peer_pid_from_socket(&control),
            Some(std::process::id())
        );
        let latest = request_stop(&record).unwrap();
        drop(listener);
        retire_selected(&record).unwrap();
        assert!(admit_resume(id, ResumeAuthority::Explicit, &Some(first)).is_err());
        assert!(stop_requested(id));
        admit_resume(id, ResumeAuthority::Explicit, &Some(latest)).unwrap();
        let listener = register_runner(&replacement).unwrap();
        assert!(!stop_requested(id));
        assert_eq!(load(id).unwrap().unwrap().generation, 2);
        let newer = request_stop(&replacement).unwrap();
        assert!(retire_selected(&record).is_err());
        assert_eq!(stop_intent(id).unwrap(), Some(newer));
        assert_eq!(load(id).unwrap().unwrap().generation, 2);
        drop(listener);
    }

    #[test]
    #[serial]
    fn malformed_record_remains_an_admission_fence_until_confirmed_recovery() {
        with_temp_home(|| {
            let id = "corrupt";
            let path = record_path(id).unwrap();
            let bytes = b"{\"pid\":";
            std::fs::write(&path, bytes).unwrap();
            let socket = socket_path_for(id).unwrap();
            let control = crate::process::worker::control_socket_sibling(&socket);
            let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
            let replacement = new_record(id, std::process::id(), socket);
            assert!(load_strict(id).is_err());
            assert!(save(&replacement).is_err());
            assert!(delete(id).is_err());
            assert!(recover_worker(id).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                crate::process::worker::peer_pid_from_socket(&control),
                Some(std::process::id())
            );
            let proof = workers_dir().unwrap().join(".unreadable.prime-proof");
            std::fs::write(&proof, b"unidentified native ownership").unwrap();
            drop(listener);
            recover_worker(id).unwrap();
            assert!(!path.exists());
            assert!(!control.exists());
            assert_eq!(
                std::fs::read(proof).unwrap(),
                b"unidentified native ownership"
            );
            save(&replacement).unwrap();
            assert_eq!(load(id).unwrap().unwrap().pid, replacement.pid);
        });
    }

    #[test]
    #[serial]
    fn retirement_error_preserves_the_selected_record_until_socket_disposal_succeeds() {
        with_temp_home(|| {
            let id = "retire-error";
            let socket = socket_path_for(id).unwrap();
            let record = new_record(id, i32::MAX as u32, socket.clone());
            save(&record).unwrap();
            let control = crate::process::worker::control_socket_sibling(&socket);
            std::fs::create_dir(&control).unwrap();
            assert!(retire_selected(&record).is_err());
            assert_eq!(load(id).unwrap().unwrap().started_at, record.started_at);
            std::fs::remove_dir(control).unwrap();
            retire_selected(&record).unwrap();
            assert!(load(id).unwrap().is_none());
        });
    }
    #[test]
    #[serial]
    fn confirmed_recovery_preserves_evidence_until_socket_disposal_succeeds() {
        with_temp_home(|| {
            for prime in [false, true] {
                let id = if prime {
                    "recover-prime-error"
                } else {
                    "recover-corrupt-error"
                };
                let path = record_path(id).unwrap();
                let socket = socket_path_for(id).unwrap();
                let mut record = new_record(id, i32::MAX as u32, socket.clone());
                record.agent_key = "prime-agent".into();
                let bytes = if prime {
                    serde_json::to_vec(&record).unwrap()
                } else {
                    b"{".to_vec()
                };
                std::fs::write(&path, &bytes).unwrap();
                let control = crate::process::worker::control_socket_sibling(&socket);
                std::fs::create_dir(&control).unwrap();
                let recover = || {
                    if prime {
                        recover_untracked_prime(&record)
                    } else {
                        recover_worker(id)
                    }
                };
                assert!(recover().is_err());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                std::fs::remove_dir(control).unwrap();
                recover().unwrap();
                assert!(!path.exists());
            }
        });
    }
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn runner_settlement_accepts_unreaped_leaders_but_still_terminates_live_group_members() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        use std::time::{Duration, Instant};
        struct OwnedGroup(std::process::Child);
        impl Drop for OwnedGroup {
            fn drop(&mut self) {
                crate::process::worker::kill_process_group(self.0.id());
                let _ = self.0.wait();
            }
        }
        for member in [false, true] {
            let script = if member {
                "sleep 600 & printf 'ready\n'; read -r ignored"
            } else {
                "printf 'ready\n'; read -r ignored"
            };
            let mut leader = OwnedGroup(
                std::process::Command::new("bash")
                    .args(["-c", script])
                    .process_group(0)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let pid = leader.0.id();
            let mut line = String::new();
            std::io::BufReader::new(leader.0.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "ready\n");
            drop(leader.0.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
                let offset = stat.windows(2).rposition(|pair| pair == b") ").unwrap();
                if stat[offset + 2] == b'Z' {
                    break;
                }
                assert!(Instant::now() < deadline, "leader did not exit");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(is_pid_alive(pid));
            assert_eq!(crate::process::runner_group_executing(pid).unwrap(), member);
            let record = new_record("owned-group", pid, PathBuf::from("/tmp/unused-control"));
            settle_runner(&record, Duration::from_millis(100))
                .await
                .unwrap();
            assert!(!crate::process::runner_group_executing(pid).unwrap());
            assert!(is_pid_alive(pid));
        }
    }

    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn with_temp_home<F: FnOnce()>(f: F) {
        // Keep worker socket paths below macOS sun_path limits.
        let tmp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        f();
    }

    #[test]
    #[serial]
    fn prime_recovery_requires_untracked_dead_exact_generation() {
        with_temp_home(|| {
            let mut selected = new_record("recovery-prime", i32::MAX as u32, "/tmp/unused.sock");
            selected.agent_key = "prime-agent".into();
            selected.generation = 7;
            for case in [
                "live",
                "tracked",
                "unattested",
                "non-prime",
                "superseded",
                "legacy",
            ] {
                let mut current = selected.clone();
                match case {
                    "live" => current.pid = std::process::id(),
                    "tracked" => {
                        current.prime_runtime = Some(
                            crate::process::prime::PrimeRuntime::new(
                                "/bin/false".into(),
                                "/tmp/store".into(),
                                None,
                            )
                            .unwrap(),
                        )
                    }
                    "unattested" => {
                        let mut runtime = crate::process::prime::PrimeRuntime::new(
                            "/bin/false".into(),
                            "/tmp/store".into(),
                            None,
                        )
                        .unwrap();
                        runtime.proof_path = None;
                        current.prime_runtime = Some(runtime);
                    }
                    "non-prime" => current.agent_key = "aoe-agent".into(),
                    "superseded" => current.generation += 1,
                    _ => {}
                }
                save(&current).unwrap();
                let snapshot = if case == "superseded" {
                    &selected
                } else {
                    &current
                };
                let result = recover_untracked_prime(snapshot);
                if case == "legacy" || case == "unattested" {
                    result.unwrap();
                    assert!(load(&current.session_id).unwrap().is_none());
                } else {
                    assert!(result.is_err(), "{case}");
                    let retained = load(&current.session_id).unwrap().unwrap();
                    assert_eq!(
                        (retained.pid, retained.generation),
                        (current.pid, current.generation)
                    );
                }
            }
        });
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
    fn empty_stored_acp_session_id_is_rejected_without_data_loss() {
        with_temp_home(|| {
            let mut rec = new_record("sess-empty-acp", 1, "/tmp/sess-empty-acp.sock");
            rec.stored_acp_session_id = Some("initial-acp".into());
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
    fn mark_attached_clears_detached() {
        with_temp_home(|| {
            let mut rec = new_record("x", 1, "/tmp/x.sock");
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
            let rec = new_record("term-dead", 2_000_000_000, "/tmp/term-dead.sock");
            save(&rec).unwrap();
            assert!(record_path("term-dead").unwrap().exists());
            terminate("term-dead").unwrap();
            assert!(!record_path("term-dead").unwrap().exists());
        });
    }

    #[test]
    #[serial]
    fn terminate_missing_entry_is_noop() {
        with_temp_home(|| {
            terminate("does-not-exist").unwrap();
            assert!(!record_path("does-not-exist").unwrap().exists());
        });
    }

    #[test]
    #[serial]
    fn pid_source_for_prefers_record_pid_when_load_ok_some() {
        with_temp_home(|| {
            let rec = new_record("sess-ok-some", 4242, "/tmp/unused");
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

    #[cfg(unix)]
    #[test]
    #[serial]
    fn pid_source_for_falls_back_to_control_socket_on_load_err() {
        with_temp_home(|| {
            let session_id = "sess-load-err";
            let rec = new_record(session_id, 4242, socket_path_for(session_id).unwrap());
            save(&rec).unwrap();
            let rec_path = record_path(session_id).unwrap();
            // A directory keeps path.exists() true while std::fs::read fails, even for root.
            std::fs::remove_file(&rec_path).unwrap();
            std::fs::create_dir(&rec_path).unwrap();
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
    #[serial]
    #[cfg(unix)]
    fn live_legacy_records_are_live_but_stale_and_terminate_reaps_them() {
        use std::os::unix::process::CommandExt as _;

        with_temp_home(|| {
            for version in [1, 3] {
                // Isolate the stand-in runner's process group.
                let mut victim = KillOnDrop(
                    std::process::Command::new("sleep")
                        .arg("60")
                        .process_group(0)
                        .spawn()
                        .expect("spawn stand-in runner"),
                );
                let session_id = format!("v{version}sess");
                let sock = workers_dir().unwrap().join(format!("{session_id}.sock"));
                let mut rec = new_record(&session_id, victim.0.id(), sock);
                rec.runner_version = version;
                // Legacy relay and control-only runners use different socket paths.
                std::fs::write(expected_socket(&rec), b"").unwrap();
                save(&rec).unwrap();

                assert!(
                    is_record_live(&rec),
                    "a live legacy runner must not read as dead: its record holds the only copy of the pid"
                );
                assert!(
                    !is_runner_current(&rec),
                    "a legacy runner is a generation behind, so the reconciler must replace it"
                );

                terminate(&session_id).unwrap();

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
                    "terminate must signal the live legacy runner, not orphan it"
                );
                assert!(!record_path(&session_id).unwrap().exists());
            }
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
            let rec = new_record("g", 41, socket).with_generation(3);
            save(&rec).unwrap();
            assert!(
                delete_if_owned_by(
                    "g",
                    crate::acp::runner_lifecycle::RunnerIdentity {
                        pid: 40,
                        generation: 3,
                        ..crate::acp::runner_lifecycle::RunnerIdentity::from_record(&rec)
                    }
                ),
                "other pid: settled without touching"
            );
            assert!(load("g").unwrap().is_some());
            assert!(
                delete_if_owned_by(
                    "g",
                    crate::acp::runner_lifecycle::RunnerIdentity {
                        generation: 4,
                        ..crate::acp::runner_lifecycle::RunnerIdentity::from_record(&rec)
                    }
                ),
                "other generation: settled, kept"
            );
            assert!(load("g").unwrap().is_some());
            assert!(delete_if_owned_by(
                "g",
                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&rec)
            ));
            assert!(load("g").unwrap().is_none());
            assert!(
                delete_if_owned_by(
                    "g",
                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(&rec)
                ),
                "missing record is settled"
            );
        });
    }
    #[test]
    #[serial]
    #[cfg(unix)]
    fn deferred_termination_preserves_a_replacement_generation() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        with_temp_home(|| {
            let mut victim = KillOnDrop(
                std::process::Command::new("sh")
                    .args(["-c", "printf 'ready\n'; exec sleep 60"])
                    .stdout(std::process::Stdio::piped())
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let mut ready = String::new();
            std::io::BufReader::new(victim.0.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            let original =
                new_record("deferred", victim.0.id(), "/tmp/deferred.sock").with_generation(1);
            let replacement = original.clone().with_generation(2);
            save(&replacement).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(runtime
                .block_on(terminate_record_and_wait(original, ()))
                .is_err());
            assert!(victim.0.try_wait().unwrap().is_none());
            assert_eq!(load("deferred").unwrap().unwrap().generation, 2);
        });
    }
}

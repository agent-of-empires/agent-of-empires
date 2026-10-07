//! On-disk registry of detached ACP workers at `<app_dir>/acp-workers/<session_id>.json` (mode 0600).
//! Writes and owned deletes serialize on a per-session lock so a superseded runner cannot
//! unlink its replacement's record.

use std::io::{Read as _, Write as _};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SocketEndpointIdentity(crate::session::DirectoryIdentity);

impl SocketEndpointIdentity {
    pub fn is_durable(&self) -> bool {
        self.0.is_durable()
    }

    pub(crate) fn observe(path: &Path) -> Result<Self> {
        use std::os::unix::fs::FileTypeExt;
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(metadata.file_type().is_socket(), "endpoint is not a socket");
        Ok(Self(crate::session::DirectoryIdentity::from_metadata(
            &metadata,
        )))
    }

    pub(crate) fn matches_metadata(&self, metadata: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::FileTypeExt;
        metadata.file_type().is_socket()
            && crate::session::DirectoryIdentity::from_metadata(metadata) == self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Actual runner birth proof; missing legacy evidence never grants a fresh capability.
    #[serde(default)]
    pub boot: Option<[u8; 16]>,
    #[serde(default)]
    pub incarnation: Option<crate::process::ProcessIncarnation>,
    #[serde(default)]
    pub profile_identity: Option<crate::session::DirectoryIdentity>,
    /// Control socket born with this published execution; legacy absence stays unknown.
    #[serde(default)]
    pub control_file_identity: Option<SocketEndpointIdentity>,
    pub started_at: u64,
    pub last_attached_at: Option<u64>,
    pub detached_at: Option<u64>,
    /// Exact registry file read at discovery; never serialized or captured at retirement.
    #[serde(skip)]
    pub(crate) record_file_identity: Option<crate::session::DirectoryIdentity>,
    #[serde(skip)]
    record_file_pin: Option<std::sync::Arc<CapturedRecordFile>>,
}

#[derive(Debug)]
struct CapturedRecordFile {
    identity: crate::session::DirectoryIdentity,
    _file: std::fs::File,
}

impl PartialEq for CapturedRecordFile {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl Eq for CapturedRecordFile {}

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
            boot: None,
            incarnation: None,
            profile_identity: None,
            control_file_identity: None,
            started_at: now_secs(),
            last_attached_at: None,
            detached_at: None,
            record_file_identity: None,
            record_file_pin: None,
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

#[cfg(unix)]
pub(crate) struct BoundEndpoint {
    session_id: String,
    path: PathBuf,
    identity: Option<SocketEndpointIdentity>,
    listener: std::sync::Arc<tokio::net::UnixListener>,
}

#[cfg(unix)]
impl BoundEndpoint {
    pub(crate) fn bind(session_id: &str, path: &Path) -> Result<Self> {
        with_registry_lock(session_id, || Self::bind_unlocked(session_id, path))
    }

    fn bind_unlocked(session_id: &str, path: &Path) -> Result<Self> {
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding {} without replacing an endpoint", path.display()))?;
        listener.set_nonblocking(true)?;
        let identity = SocketEndpointIdentity::observe(path)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        Ok(Self {
            session_id: session_id.to_owned(),
            path: path.to_owned(),
            identity: Some(identity),
            listener: std::sync::Arc::new(listener),
        })
    }

    pub(crate) fn identity(&self) -> SocketEndpointIdentity {
        self.identity.expect("live endpoint custody")
    }

    pub(crate) fn listener(&self) -> std::sync::Arc<tokio::net::UnixListener> {
        std::sync::Arc::clone(&self.listener)
    }

    pub(crate) fn cleanup(&mut self) {
        let Some(identity) = self.identity.take() else {
            return;
        };
        if let Err(error) = with_registry_lock(&self.session_id, || {
            remove_endpoint_unlocked(&self.path, &identity)
        }) {
            warn!(session_id = %self.session_id, %error, "retaining endpoint after failed cleanup");
        }
    }
}

#[cfg(unix)]
impl Drop for BoundEndpoint {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(unix)]
fn remove_endpoint_unlocked(path: &Path, identity: &SocketEndpointIdentity) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if identity.matches_metadata(&metadata) => {
            std::fs::remove_file(path)?;
            crate::session::sync_parent_directory(path)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn retire_endpoint(
    session_id: &str,
    path: &Path,
    identity: &SocketEndpointIdentity,
) -> Result<()> {
    anyhow::ensure!(
        identity.is_durable(),
        "endpoint retirement lacks durable birth evidence"
    );
    with_registry_lock(session_id, || remove_endpoint_unlocked(path, identity))
}

/// Publishes the listener and record under the same fence as owned cleanup.
#[cfg(unix)]
pub(crate) fn publish_control_listener(
    record: &mut WorkerRecord,
    socket: &Path,
) -> Result<BoundEndpoint> {
    let lock = acquire_registry_lock(&record.session_id)?;
    let mut endpoint = None;
    let publish = (|| {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            record.launch_nonce.is_some()
                && record.boot.is_some()
                && record.incarnation.is_some()
                && record
                    .profile_identity
                    .is_some_and(|identity| identity.is_durable()),
            "runner publication requires complete birth evidence"
        );
        anyhow::ensure!(
            load_strict_unlocked(&record.session_id)?.is_none(),
            "a registry record appeared before native publication"
        );
        endpoint = Some(BoundEndpoint::bind_unlocked(&record.session_id, socket)?);
        let identity = endpoint.as_ref().unwrap().identity();
        anyhow::ensure!(
            identity.is_durable(),
            "native control endpoint birth time is unavailable; fresh publication is unproven"
        );
        record.control_file_identity = Some(identity);
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .context("securing runner control socket")
            .and_then(|()| {
                let bytes =
                    serde_json::to_vec_pretty(record).context("serializing worker record")?;
                let pin = write_record_bytes_unlocked(&record.session_id, &bytes)?;
                record.record_file_identity = Some(pin.identity);
                record.record_file_pin = Some(std::sync::Arc::new(pin));
                Ok(())
            })
    })();
    finish_registry_operation(lock, &record.session_id, publish)?;
    Ok(endpoint.expect("published endpoint custody"))
}

fn save_unlocked(record: &WorkerRecord) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record).context("serializing worker record")?;
    write_record_bytes_unlocked(&record.session_id, &bytes).map(|_| ())
}

fn write_record_bytes_unlocked(id: &str, bytes: &[u8]) -> Result<CapturedRecordFile> {
    let dir = workers_dir()?;
    let final_path = dir.join(format!("{id}.json"));
    let tmp_path = dir.join(format!("{id}.json.tmp"));
    let mut file = std::fs::File::create(&tmp_path)
        .with_context(|| format!("opening tmp record at {}", tmp_path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing tmp record at {}", tmp_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("renaming tmp record to {}", final_path.display()))?;
    Ok(CapturedRecordFile {
        identity: crate::session::DirectoryIdentity::from_metadata(&file.metadata()?),
        _file: file,
    })
}

fn acquire_registry_lock(session_id: &str) -> Result<std::fs::File> {
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

fn finish_registry_operation<T>(
    lock_file: std::fs::File,
    session_id: &str,
    result: Result<T>,
) -> Result<T> {
    let unlock = fs2::FileExt::unlock(&lock_file)
        .with_context(|| format!("unlocking worker registry entry {session_id}"));
    drop(lock_file);
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn with_registry_lock<T>(session_id: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = acquire_registry_lock(session_id)?;
    let result = operation();
    finish_registry_operation(lock, session_id, result)
}

pub fn load(session_id: &str) -> Result<Option<WorkerRecord>> {
    let path = record_path(session_id)?;
    match read_record(&path) {
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            warn!(target: "acp.registry", path = %path.display(),
                "failed to parse worker record: {error}; treating as missing");
            Ok(None)
        }
        result => result,
    }
}
/// [`load`] without folding a record it cannot read into "no runner": an
/// unreadable record answers `Err`, which a caller about to destroy a checkout
/// has to refuse on rather than treat as an absent session.
pub fn load_strict(session_id: &str) -> Result<Option<WorkerRecord>> {
    load_strict_unlocked(session_id)
}

fn load_strict_unlocked(session_id: &str) -> Result<Option<WorkerRecord>> {
    read_record(&record_path(session_id)?)
}

fn read_discovered_record<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<(T, std::sync::Arc<CapturedRecordFile>)>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    anyhow::ensure!(
        before.is_file(),
        "registry record is not a regular file: {}",
        path.display()
    );
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "opened registry record is not regular: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let after =
        std::fs::symlink_metadata(path).with_context(|| format!("verifying {}", path.display()))?;
    anyhow::ensure!(
        after.is_file()
            && crate::session::DirectoryIdentity::from_metadata(&metadata)
                == crate::session::DirectoryIdentity::from_metadata(&before)
            && crate::session::DirectoryIdentity::from_metadata(&metadata)
                == crate::session::DirectoryIdentity::from_metadata(&after),
        "registry record changed while reading {}",
        path.display()
    );
    let record =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let pin = std::sync::Arc::new(CapturedRecordFile {
        identity: crate::session::DirectoryIdentity::from_metadata(&metadata),
        _file: file,
    });
    Ok(Some((record, pin)))
}

fn read_record(path: &Path) -> Result<Option<WorkerRecord>> {
    let Some((mut record, pin)) = read_discovered_record::<WorkerRecord>(path)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        path.file_stem().and_then(|stem| stem.to_str()) == Some(record.session_id.as_str()),
        "registry record session identity differs from its discovered namespace"
    );
    record.record_file_identity = Some(pin.identity);
    record.record_file_pin = Some(pin);
    Ok(Some(record))
}

/// One-time schema normalization preserves weak birth data without discovering new authority.
pub(crate) fn migrate_birth_stamps(
    transform: impl Fn(&mut serde_json::Value) -> bool,
) -> Result<()> {
    let directory = crate::session::get_app_dir()?.join("acp-workers");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_session_id(id).is_err() {
            continue;
        }
        with_registry_lock(id, || {
            let Some((mut value, pin)) = read_discovered_record::<serde_json::Value>(&path)? else {
                return Ok(());
            };
            anyhow::ensure!(
                value.get("session_id").and_then(|value| value.as_str()) == Some(id),
                "registry migration discovered a different session identity"
            );
            if transform(&mut value) {
                let current = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(
                    crate::session::DirectoryIdentity::from_metadata(&current) == pin.identity,
                    "registry file changed before birth normalization"
                );
                let bytes = serde_json::to_vec_pretty(&value)?;
                write_record_bytes_unlocked(id, &bytes)?;
            }
            Ok(())
        })?;
    }
    Ok(())
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
        match read_record(&path) {
            Ok(Some(record)) => out.push(record),
            Ok(None) => {}
            Err(error) => warn!(target: "acp.registry", path = %path.display(),
                "skipping unreadable worker record: {error}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn delete(session_id: &str) -> Result<()> {
    with_registry_lock(session_id, || delete_unlocked(session_id))
}

fn delete_unlocked(session_id: &str) -> Result<()> {
    let path = record_path(session_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("retiring {}", path.display())),
    }
    Ok(())
}

fn update_if_owned<T>(
    expected: &mut WorkerRecord,
    update: impl FnOnce(&mut WorkerRecord) -> T,
    rollback: impl FnOnce(&mut WorkerRecord, T),
) -> Result<bool> {
    let identity = crate::acp::runner_lifecycle::RunnerIdentity {
        pid: expected.pid,
        generation: expected.generation,
        launch_nonce: expected.launch_nonce,
        incarnation: expected.incarnation,
        profile_identity: expected.profile_identity,
        boot: expected.boot,
    };
    anyhow::ensure!(
        identity.birth_is_complete() && expected.record_file_pin.is_some(),
        "self mutation has no captured full native record-file custody"
    );
    let lock = acquire_registry_lock(&expected.session_id)?;
    let result = (|| {
        let Some(current) = load_strict_unlocked(&expected.session_id)? else {
            return Ok(false);
        };
        if current != *expected || !identity.matches_record(&current) {
            return Ok(false);
        }
        let previous = update(expected);
        let written = (|| {
            let bytes =
                serde_json::to_vec_pretty(expected).context("serializing owned worker update")?;
            write_record_bytes_unlocked(&expected.session_id, &bytes)
        })();
        match written {
            Ok(pin) => {
                expected.record_file_identity = Some(pin.identity);
                expected.record_file_pin = Some(std::sync::Arc::new(pin));
                Ok(true)
            }
            Err(error) => {
                rollback(expected, previous);
                Err(error)
            }
        }
    })();
    finish_registry_operation(lock, &expected.session_id, result)
}

pub fn mark_attached(expected: &mut WorkerRecord) -> Result<()> {
    let updated = update_if_owned(
        expected,
        |record| {
            let previous = (record.last_attached_at, record.detached_at);
            record.last_attached_at = Some(now_secs());
            record.detached_at = None;
            previous
        },
        |record, previous| {
            record.last_attached_at = previous.0;
            record.detached_at = previous.1;
        },
    )?;
    anyhow::ensure!(updated, "runner no longer owns its captured registry file");
    Ok(())
}

pub fn mark_detached(expected: &mut WorkerRecord) -> Result<()> {
    let updated = update_if_owned(
        expected,
        |record| record.detached_at.replace(now_secs()),
        |record, previous| {
            record.detached_at = previous;
        },
    )?;
    anyhow::ensure!(updated, "runner no longer owns its captured registry file");
    Ok(())
}

pub fn update_stored_acp_session_id(expected: &mut WorkerRecord, acp_id: &str) -> Result<()> {
    anyhow::ensure!(!acp_id.is_empty(), "ACP session id must not be empty");
    let updated = update_if_owned(
        expected,
        |record| record.stored_acp_session_id.replace(acp_id.to_owned()),
        |record, previous| {
            record.stored_acp_session_id = previous;
        },
    )?;
    anyhow::ensure!(updated, "runner no longer owns its captured registry file");
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

#[cfg(unix)]
fn retire_owned_control(record: &WorkerRecord) -> Result<()> {
    let Some(identity) = record
        .control_file_identity
        .filter(|identity| identity.is_durable())
    else {
        return Ok(());
    };
    remove_endpoint_unlocked(&expected_socket(record), &identity)
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

/// Retire only the born record actually discovered before settlement.
/// A peer rewrite, even for the same native execution, keeps its new file custody.
pub fn delete_if_owned_by(expected: &WorkerRecord) -> bool {
    let identity = crate::acp::runner_lifecycle::RunnerIdentity {
        pid: expected.pid,
        generation: expected.generation,
        launch_nonce: expected.launch_nonce,
        incarnation: expected.incarnation,
        profile_identity: expected.profile_identity,
        boot: expected.boot,
    };
    if !identity.birth_is_complete() || expected.record_file_pin.is_none() {
        return false;
    }
    with_registry_lock(&expected.session_id, || match load_strict_unlocked(&expected.session_id)? {
        None => Ok(true),
        Some(current) if current == *expected && identity.matches_record(&current) => {
            #[cfg(unix)] retire_owned_control(&current)?;
            delete_unlocked(&expected.session_id).map(|()| true)
        }
        Some(current) => {
            debug!(target: "acp.registry", session = %expected.session_id, current_pid = current.pid,
                "preserving registry file outside captured birth and discovery custody");
            Ok(false)
        }
    }).unwrap_or(false)
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

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn bound_endpoint_retains_its_listener_and_preserves_replacement() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = TempDir::with_prefix_in("aoe-endpoint-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let path =
            crate::process::worker::control_socket_sibling(&socket_path_for("custody").unwrap());
        let mut endpoint = BoundEndpoint::bind("custody", &path).unwrap();
        let identity = endpoint.identity();
        let listener = endpoint.listener();
        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (mut original, _) = listener.accept().await.unwrap();
        drop(listener);
        assert!(BoundEndpoint::bind("custody", &path).is_err());
        assert_eq!(SocketEndpointIdentity::observe(&path).unwrap(), identity);
        std::fs::remove_file(&path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_ne!(SocketEndpointIdentity::observe(&path).unwrap(), identity);
        endpoint.cleanup();
        drop(endpoint);
        original.write_all(b"owned").await.unwrap();
        let mut reply = [0; 5];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"owned");
        assert!(path.exists(), "cleanup must preserve the peer endpoint");
        drop(replacement);
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
            let mut original = load_strict("sess-empty-acp").unwrap().unwrap();
            assert!(update_stored_acp_session_id(&mut original, "").is_err());
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
    fn legacy_discovery_preserves_identical_peer_rewrite_and_unknown_endpoint() {
        with_temp_home(|| {
            let session_id = "replacement";
            let socket = socket_path_for(session_id).unwrap();
            touch_live_socket(&socket);
            let control = crate::process::worker::control_socket_sibling(&socket);
            let record = new_record(session_id, 111, socket);
            save(&record).unwrap();
            let first = load_strict(session_id).unwrap().unwrap();
            // A peer writes identical metadata into a different file inode.
            save(&first).unwrap();
            let second = load_strict(session_id).unwrap().unwrap();
            assert!(!delete_if_owned_by(&first));
            assert!(load_strict(session_id).unwrap().is_some());
            assert!(control.exists());
            assert!(
                !delete_if_owned_by(&second),
                "legacy metadata grants no native stop or endpoint authority"
            );
            assert!(load_strict(session_id).unwrap().is_some());
            assert!(control.exists(), "unknown endpoint must survive discovery");
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
}

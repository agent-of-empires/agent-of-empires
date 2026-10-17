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
    pub started_at: u64,
    pub last_attached_at: Option<u64>,
    pub detached_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PurgeFence {
    source_profile: String,
    lifecycle_generation: u64,
    /// Last runner identity observed while this purge fence was active. The
    /// runner may remove its registry record before its whole process group
    /// exits, so recovery must retain this evidence across retries.
    #[serde(default)]
    runner_pid: Option<u32>,
    #[serde(default)]
    runner_generation: Option<u64>,
    #[serde(default)]
    runner_identity: Option<crate::process::ProcessBirthIdentity>,
    #[serde(default)]
    runner_record: Option<serde_json::Value>,
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

#[cfg(test)]
pub(crate) fn seed_settled_stop_intent(session_id: &str) -> Result<()> {
    with_registry_lock(session_id, || {
        let intent = StopIntent {
            pid: std::process::id(),
            generation: 1,
            token: uuid::Uuid::new_v4().simple().to_string(),
        };
        crate::session::atomic_write(&stop_path(session_id)?, &serde_json::to_vec(&intent)?)
    })
}

pub(crate) fn admit_resume(
    session_id: &str,
    authority: ResumeAuthority,
    observed: &Option<StopIntent>,
) -> Result<()> {
    with_registry_lock(session_id, || {
        let current = stop_intent(session_id)?;
        if current.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            authority == ResumeAuthority::Explicit && &current == observed,
            "worker stop intent fences automatic or superseded resume"
        );
        anyhow::ensure!(
            load_strict_unlocked(session_id)?.is_none(),
            "worker shutdown is not settled; resume remains fenced"
        );
        std::fs::remove_file(stop_path(session_id)?)?;
        std::fs::File::open(workers_dir()?)?.sync_all()?;
        Ok(())
    })
}

pub(crate) fn retire_selected(selected: &WorkerRecord) -> Result<()> {
    with_registry_lock(&selected.session_id, || {
        anyhow::ensure!(
            load_purge_fence_unlocked(&selected.session_id)?.is_none(),
            "worker retirement is fenced by an active session purge"
        );
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
        ensure_publication_unfenced(&record.session_id)?;
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
    let (path, selected) = with_registry_lock(session_id, || {
        let record = record_path(session_id)?;
        match std::fs::read(&record) {
            Ok(bytes) => {
                #[cfg(unix)]
                if let Some(orphan) = load_orphan_tracking_unlocked(session_id)? {
                    let current: WorkerRecord = serde_json::from_slice(&bytes)
                        .context("parsing worker record alongside orphan evidence")?;
                    anyhow::ensure!(
                        same_runner(&current, &orphan),
                        "registry and orphan recovery identities disagree"
                    );
                }
                Ok((record, bytes))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    load_orphan_tracking_unlocked(session_id)?
                        .context("worker ownership evidence is absent")?;
                    let orphan = orphan_tracking_path(session_id)?;
                    let bytes = std::fs::read(&orphan)?;
                    Ok((orphan, bytes))
                }
                #[cfg(not(unix))]
                Err(error.into())
            }
            Err(error) => Err(error.into()),
        }
    })?;
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
        }
        // An unattested PID may have been reused. Explicit external confirmation,
        // not the current occupant, is the authority for retiring that evidence.
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
        #[cfg(unix)]
        {
            let orphan = load_orphan_tracking_unlocked(session_id)?;
            if let Some(selected) = record.as_ref() {
                if let Some(current) = orphan.as_ref() {
                    anyhow::ensure!(
                        same_runner(selected, current),
                        "orphan recovery identity changed"
                    );
                }
                if path == orphan_tracking_path(session_id)? {
                    if let Some(current) = load_strict_unlocked(session_id)? {
                        anyhow::ensure!(
                            same_runner(selected, &current),
                            "registry recovery identity changed"
                        );
                    }
                }
            } else {
                anyhow::ensure!(
                    orphan.is_none(),
                    "orphan ownership appeared during malformed-record recovery"
                );
            }
        }
        let control = crate::process::worker::control_socket_sibling(&socket_path_for(session_id)?);
        anyhow::ensure!(
            crate::process::worker::peer_pid_from_socket(&control).is_none(),
            "worker control socket still has a live owner; recovery refused"
        );
        if let Some(mut fence) = load_purge_fence_unlocked(session_id)? {
            fence.runner_pid = None;
            fence.runner_generation = None;
            fence.runner_identity = None;
            fence.runner_record = None;
            save_purge_fence_unlocked(session_id, &fence)?;
        }
        #[cfg(unix)]
        if load_orphan_tracking_unlocked(session_id)?.is_some() {
            remove_orphan_tracking_unlocked(session_id)?;
        }
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

fn purge_fence_path(session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    Ok(workers_dir()?.join(format!("{session_id}.purge-fence")))
}

fn load_purge_fence_unlocked(session_id: &str) -> Result<Option<PurgeFence>> {
    let path = purge_fence_path(session_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing purge fence {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading purge fence {}", path.display())),
    }
}

fn save_purge_fence_unlocked(session_id: &str, fence: &PurgeFence) -> Result<()> {
    let path = purge_fence_path(session_id)?;
    let bytes = serde_json::to_vec(fence).context("serializing ACP purge fence")?;
    crate::session::atomic_write_verified(&path, &bytes)?;
    crate::session::sync_parent_directory(&path)
}

fn profile_reference_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for item in std::fs::read_dir(workers_dir()?)? {
        let item = item?;
        let path = item.path();
        if !path
            .extension()
            .is_some_and(|ext| ext == "json" || ext == "orphan-stop" || ext == "purge-fence")
        {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

fn read_profile_reference(path: &Path) -> Result<Option<serde_json::Value>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "registry profile reference is not a regular file: {}",
        path.display()
    );
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(value.is_object(), "invalid profile registry reference");
    anyhow::ensure!(
        value
            .get("source_profile")
            .is_none_or(|profile| profile.is_null() || profile.is_string()),
        "invalid registry profile"
    );
    Ok(Some(value))
}

pub(crate) fn profile_reference_sessions(profile: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for path in profile_reference_paths()? {
        let id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .context("invalid registry filename")?;
        validate_session_id(id)?;
        with_registry_lock(id, || {
            if let Some(value) = read_profile_reference(&path)? {
                if value
                    .get("source_profile")
                    .and_then(|profile| profile.as_str())
                    == Some(profile)
                {
                    ids.push(id.to_owned());
                }
            }
            Ok(())
        })?;
    }
    Ok(ids)
}

pub(crate) fn acquire_profile_deletion_registry_guards(
    profile: &str,
    session_ids: &[String],
) -> Result<Vec<std::fs::File>> {
    let mut guards = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        let guard = acquire_registry_lock(session_id)?;
        if let Some(record) = load_strict_unlocked(session_id)? {
            anyhow::ensure!(
                record.session_id == *session_id,
                "worker registry session identity mismatch"
            );
            if let Some(source_profile) = record.source_profile.as_deref() {
                anyhow::ensure!(
                    source_profile == profile,
                    "worker registry references a different profile"
                );
            }
            anyhow::bail!("profile has worker ownership; stop or recover its session before deleting the profile");
        }
        #[cfg(unix)]
        if let Some(orphan) = load_orphan_tracking_unlocked(session_id)? {
            anyhow::ensure!(
                orphan
                    .source_profile
                    .as_deref()
                    .is_none_or(|source_profile| source_profile == profile),
                "orphan tracking references a different profile"
            );
            anyhow::bail!("profile has pending orphan cleanup");
        }
        if let Some(fence) = load_purge_fence_unlocked(session_id)? {
            anyhow::ensure!(
                fence.source_profile == profile,
                "purge fence references a different profile"
            );
            anyhow::bail!("profile has a pending purge fence");
        }
        guards.push(guard);
    }
    Ok(guards)
}

pub(crate) fn retarget_profile_references(
    id: &str,
    old: &str,
    new: &str,
    target: &str,
) -> Result<()> {
    with_registry_lock(id, || {
        let dir = workers_dir()?;
        for extension in ["json", "orphan-stop", "purge-fence"] {
            let path = dir.join(format!("{id}.{extension}"));
            let Some(mut value) = read_profile_reference(&path)? else {
                continue;
            };
            let profile = value
                .get("source_profile")
                .and_then(|profile| profile.as_str());
            if profile
                .is_some_and(|profile| (profile == old || profile == new) && profile != target)
            {
                value["source_profile"] = serde_json::Value::String(target.to_owned());
                if let Some(record) = value
                    .get_mut("runner_record")
                    .filter(|record| record.is_object())
                {
                    record["source_profile"] = serde_json::Value::String(target.to_owned());
                }
                crate::session::atomic_write_verified(&path, &serde_json::to_vec(&value)?)?;
            }
            crate::session::sync_parent_directory(&path)?;
        }
        Ok(())
    })
}

#[cfg(unix)]
fn orphan_tracking_path(session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    Ok(workers_dir()?.join(format!("{session_id}.orphan-stop")))
}

#[cfg(unix)]
fn validate_orphan_record(record: &WorkerRecord, session_id: &str) -> Result<()> {
    anyhow::ensure!(
        record.session_id == session_id,
        "orphan tracking session identity changed"
    );
    anyhow::ensure!(
        record.pid > 0 && record.pid <= i32::MAX as u32,
        "ACP runner PID {} is invalid",
        record.pid
    );
    Ok(())
}

#[cfg(unix)]
fn load_orphan_tracking_unlocked(session_id: &str) -> Result<Option<WorkerRecord>> {
    let path = orphan_tracking_path(session_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let record = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing orphan tracking {}", path.display()))?;
            validate_orphan_record(&record, session_id)?;
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}
pub(crate) fn load_recovery_candidate(session_id: &str) -> Result<Option<WorkerRecord>> {
    #[cfg(unix)]
    {
        with_registry_lock(session_id, || {
            let current = load_strict_unlocked(session_id)?;
            let orphan = load_orphan_tracking_unlocked(session_id)?;
            if let (Some(current), Some(orphan)) = (&current, &orphan) {
                anyhow::ensure!(
                    same_runner(current, orphan),
                    "registry and orphan recovery identities disagree"
                );
            }
            Ok(current.or(orphan))
        })
    }
    #[cfg(not(unix))]
    load(session_id)
}

#[cfg(unix)]
fn write_orphan_tracking_unlocked(record: &WorkerRecord) -> Result<()> {
    validate_orphan_record(record, &record.session_id)?;
    let path = orphan_tracking_path(&record.session_id)?;
    let bytes = serde_json::to_vec(record).context("serializing orphan stop evidence")?;
    crate::session::atomic_write_verified(&path, &bytes)?;
    crate::session::sync_parent_directory(&path)
}

#[cfg(all(test, unix))]
pub(crate) fn mark_orphan_pending_for_test(record: &WorkerRecord) -> Result<()> {
    with_registry_lock(&record.session_id, || {
        write_orphan_tracking_unlocked(record)
    })
}

#[cfg(unix)]
fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(unix)]
fn remove_orphan_tracking_unlocked(session_id: &str) -> Result<()> {
    let path = orphan_tracking_path(session_id)?;
    remove_file_if_present(&path)?;
    crate::session::sync_parent_directory(&path)
}

/// Fence runner publication and readoption before a purge can remove session resources.
pub fn fence_for_purge(
    session_id: &str,
    source_profile: &str,
    lifecycle_generation: u64,
) -> Result<()> {
    with_registry_lock(session_id, || {
        let current = load_purge_fence_unlocked(session_id)?;
        if let Some(current) = current.as_ref() {
            anyhow::ensure!(
                current.source_profile == source_profile,
                "ACP session id is already fenced by profile {}",
                current.source_profile
            );
            anyhow::ensure!(
                current.lifecycle_generation <= lifecycle_generation,
                "a newer ACP purge fence is already present"
            );
        }
        let mut fence = current.unwrap_or_else(|| PurgeFence {
            source_profile: source_profile.to_string(),
            lifecycle_generation,
            runner_pid: None,
            runner_generation: None,
            runner_identity: None,
            runner_record: None,
        });
        fence.lifecycle_generation = lifecycle_generation;
        #[cfg(unix)]
        let mut orphan = load_orphan_tracking_unlocked(session_id)?;
        if fence.runner_pid.is_none() {
            if let Some(record) = load_strict_unlocked(session_id)? {
                fence.runner_pid = Some(record.pid);
                fence.runner_generation = Some(record.generation);
                fence.runner_identity = record.runner_identity.clone();
                fence.runner_record = Some(serde_json::to_value(&record)?);
            } else {
                #[cfg(unix)]
                {
                    let base = socket_path_for(session_id)?;
                    let control = crate::process::worker::control_socket_sibling(&base);
                    let control_pid = crate::process::worker::peer_pid_from_socket(&control);
                    let base_pid = crate::process::worker::peer_pid_from_socket(&base);
                    anyhow::ensure!(
                        !matches!((control_pid, base_pid), (Some(left), Some(right)) if left != right),
                        "ACP runner socket identities disagree"
                    );
                    let peer = control_pid.or(base_pid);
                    let peer_birth = match peer {
                        Some(pid) => crate::process::process_birth_identity(pid)?,
                        None => None,
                    };
                    if let Some(tracked) = orphan.as_ref() {
                        if peer.is_some_and(|pid| pid != tracked.pid)
                            || (peer.is_some()
                                && tracked.runner_identity.is_some()
                                && peer_birth != tracked.runner_identity)
                        {
                            anyhow::ensure!(
                                !runner_group_alive(
                                    tracked.pid,
                                    tracked
                                        .runner_identity
                                        .as_ref()
                                        .map(crate::process::ProcessBirthIdentity::fingerprint),
                                ),
                                "a different orphan process group is still tracked"
                            );
                            remove_orphan_tracking_unlocked(session_id)?;
                            orphan = None;
                        }
                    }
                    if let Some(tracked) = orphan.as_ref() {
                        fence.runner_pid = Some(tracked.pid);
                        fence.runner_generation = Some(tracked.generation);
                        fence.runner_identity = tracked.runner_identity.clone();
                        fence.runner_record = Some(serde_json::to_value(tracked)?);
                    } else {
                        fence.runner_pid = peer;
                        fence.runner_generation = None;
                        fence.runner_identity = peer_birth;
                    }
                    anyhow::ensure!(
                        fence.runner_pid.is_some()
                            || (!socket_exists(&base) && !socket_exists(&control)),
                        "ACP runner socket exists but its owner identity is unknown"
                    );
                }
            }
        }
        #[cfg(unix)]
        if let Some(tracked) = orphan.as_ref() {
            let matches = fence.runner_pid == Some(tracked.pid)
                && fence
                    .runner_generation
                    .is_none_or(|generation| generation == tracked.generation)
                && fence.runner_identity == tracked.runner_identity;
            if !matches {
                anyhow::ensure!(
                    !runner_group_alive(
                        tracked.pid,
                        tracked
                            .runner_identity
                            .as_ref()
                            .map(crate::process::ProcessBirthIdentity::fingerprint),
                    ),
                    "a different orphan process group is still tracked"
                );
                remove_orphan_tracking_unlocked(session_id)?;
            }
        }
        if let Some(pid) = fence.runner_pid {
            anyhow::ensure!(
                pid > 0 && pid <= i32::MAX as u32,
                "ACP runner PID {pid} is invalid"
            );
        }
        save_purge_fence_unlocked(session_id, &fence)
    })
}

pub fn clear_purge_fence_if_owned(
    session_id: &str,
    source_profile: &str,
    lifecycle_generation: u64,
) -> Result<bool> {
    with_registry_lock(session_id, || {
        let Some(current) = load_purge_fence_unlocked(session_id)? else {
            return Ok(false);
        };
        if current.source_profile != source_profile
            || current.lifecycle_generation != lifecycle_generation
        {
            return Ok(false);
        }
        #[cfg(unix)]
        if let Some(pid) = current.runner_pid {
            if runner_group_alive(
                pid,
                current
                    .runner_identity
                    .as_ref()
                    .map(crate::process::ProcessBirthIdentity::fingerprint),
            ) {
                let matches = |record: &WorkerRecord| {
                    record.pid == pid
                        && current
                            .runner_generation
                            .is_none_or(|generation| generation == record.generation)
                        && current.runner_identity == record.runner_identity
                };
                let registered = load_strict_unlocked(session_id)?
                    .as_ref()
                    .is_some_and(matches);
                let tracked = load_orphan_tracking_unlocked(session_id)?
                    .as_ref()
                    .is_some_and(matches);
                anyhow::ensure!(
                    registered || tracked,
                    "live ACP group would lose its last durable tracking evidence"
                );
            }
        }
        let path = purge_fence_path(session_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()))
            }
        }
        crate::session::sync_parent_directory(&path)?;
        Ok(true)
    })
}

pub fn is_purge_fenced(session_id: &str) -> Result<bool> {
    Ok(load_purge_fence_unlocked(session_id)?.is_some())
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
    with_registry_lock(&record.session_id, || {
        ensure_publication_unfenced(&record.session_id)?;
        save_unlocked(record)
    })
}
fn ensure_publication_unfenced(session_id: &str) -> Result<()> {
    if let Some(fence) = load_purge_fence_unlocked(session_id)? {
        anyhow::bail!(
            "ACP runner publication is fenced by purge generation {} in profile {}",
            fence.lifecycle_generation,
            fence.source_profile
        );
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn publish_runner(
    record: &WorkerRecord,
) -> Result<(tokio::net::UnixListener, impl Drop)> {
    let profile_guard = crate::session::profile_rename::publication_guard(
        record.source_profile.as_deref(),
        &record.session_id,
    )?;
    anyhow::ensure!(
        record.pid == std::process::id() && record.runner_identity.is_some(),
        "runner publication lacks its birth identity"
    );
    let listener = register_runner(record)?;
    Ok((listener, profile_guard))
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
        .context("publishing worker record")?;
    crate::session::sync_parent_directory(&path).context("committing worker record")
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
    load_strict_unlocked(session_id)
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
    list_records(false)
}

#[cfg(unix)]
pub(crate) fn list_for_reaping() -> Result<Vec<WorkerRecord>> {
    list_records(true)
}

fn list_records(include_orphans: bool) -> Result<Vec<WorkerRecord>> {
    let dir = workers_dir()?;
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    for item in read {
        let item = match item {
            Ok(item) => item,
            Err(error) if include_orphans => {
                return Err(error).context("enumerating orphan tracking")
            }
            Err(_) => continue,
        };
        let path = item.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        let tracking = include_orphans && extension == Some("orphan-stop");
        if !tracking && extension != Some("json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if tracking => {
                return Err(error).with_context(|| format!("reading {}", path.display()))
            }
            Err(_) => continue,
        };
        match serde_json::from_slice::<WorkerRecord>(&bytes) {
            Ok(record) => {
                #[cfg(unix)]
                if tracking {
                    let id = path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .context("invalid orphan tracking filename")?;
                    validate_session_id(id)?;
                    validate_orphan_record(&record, id)?;
                }
                out.push(record);
            }
            Err(error) if tracking => {
                return Err(error).with_context(|| format!("parsing {}", path.display()))
            }
            Err(error) => {
                warn!(target: "acp.registry", path = %path.display(), "skipping unparseable worker record: {error}")
            }
        }
    }
    if include_orphans {
        out.sort_unstable_by(|left, right| {
            left.session_id
                .cmp(&right.session_id)
                .then_with(|| left.pid.cmp(&right.pid))
                .then_with(|| left.generation.cmp(&right.generation))
                .then_with(|| {
                    left.runner_identity
                        .as_ref()
                        .map(|identity| (&identity.boot_id, &identity.start_id))
                        .cmp(
                            &right
                                .runner_identity
                                .as_ref()
                                .map(|identity| (&identity.boot_id, &identity.start_id)),
                        )
                })
        });
        out.dedup_by(|left, right| {
            left.session_id == right.session_id
                && left.pid == right.pid
                && left.generation == right.generation
                && left.runner_identity == right.runner_identity
        });
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

pub(crate) fn retire_if_owned(selected: &WorkerRecord) -> Result<bool> {
    with_registry_lock(&selected.session_id, || {
        match load_strict_unlocked(&selected.session_id)? {
            Some(current) if same_runner(&current, selected) => {
                delete_unlocked(&selected.session_id)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    })
}

fn update_if_owned(
    session_id: &str,
    owner_pid: u32,
    update: impl FnOnce(&mut WorkerRecord),
) -> Result<bool> {
    with_registry_lock(session_id, || {
        if load_purge_fence_unlocked(session_id)?.is_some() {
            return Ok(false);
        }
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

#[cfg(not(unix))]
fn delete_if_absent(session_id: &str) -> Result<bool> {
    with_registry_lock(session_id, || {
        if load_strict_unlocked(session_id)?.is_some() {
            return Ok(false);
        }
        delete_unlocked(session_id)?;
        Ok(true)
    })
}

#[cfg(unix)]
fn delete_if_absent(_session_id: &str) -> Result<bool> {
    // Missing registry data does not prove ownership of leftover Unix socket paths.
    Ok(false)
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

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunnerIdentity {
    pid: u32,
    generation: Option<u64>,
    runner_identity: Option<crate::process::ProcessBirthFingerprint>,
}

#[cfg(unix)]
fn ensure_process_birth(
    pid: u32,
    expected: Option<crate::process::ProcessBirthFingerprint>,
) -> Result<()> {
    let expected = expected.context("ACP runner birth is unknown")?;
    let actual = crate::process::process_birth_identity(pid)?
        .as_ref()
        .map(crate::process::ProcessBirthIdentity::fingerprint);
    anyhow::ensure!(actual == Some(expected), "ACP runner birth changed");
    Ok(())
}

#[cfg(all(test, unix))]
thread_local! {
    static AFTER_FENCED_RUNNER_SIGNAL: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(unix)]
use crate::process::SocketArtifactIdentity;

#[cfg(unix)]
fn socket_artifact_identity(path: &Path) -> Result<Option<SocketArtifactIdentity>> {
    crate::process::socket_artifact_identity(path)
        .with_context(|| format!("inspecting socket identity of {}", path.display()))
}

#[cfg(unix)]
struct PurgeSocketEvidence {
    paths: [PathBuf; 2],
    initial: Option<[Option<SocketArtifactIdentity>; 2]>,
    pins: [Option<crate::process::SocketArtifactPin>; 2],
}

#[cfg(unix)]
impl PurgeSocketEvidence {
    fn new(session_id: &str) -> Result<Self> {
        let base = socket_path_for(session_id)?;
        let control = crate::process::worker::control_socket_sibling(&base);
        Ok(Self {
            paths: [base, control],
            initial: None,
            pins: [None, None],
        })
    }

    fn inspect(
        &self,
        pid: u32,
        fingerprint: Option<crate::process::ProcessBirthFingerprint>,
    ) -> Result<([Option<SocketArtifactIdentity>; 2], [Option<u32>; 2])> {
        let identities = [
            socket_artifact_identity(&self.paths[0])?,
            socket_artifact_identity(&self.paths[1])?,
        ];
        let peers = [
            crate::process::worker::peer_pid_from_socket(&self.paths[0]),
            crate::process::worker::peer_pid_from_socket(&self.paths[1]),
        ];
        for (index, peer) in peers.iter().enumerate() {
            anyhow::ensure!(
                peer.is_none_or(|peer| peer == pid),
                "ACP runner socket has a different owner: {}",
                self.paths[index].display()
            );
        }
        if peers.iter().any(Option::is_some) {
            ensure_process_birth(pid, fingerprint)?;
        }
        if let Some(initial) = self.initial.as_ref() {
            for (index, current) in identities.iter().enumerate() {
                anyhow::ensure!(
                    current.is_none() || current == &initial[index],
                    "ACP runner socket changed after stop verification: {}",
                    self.paths[index].display()
                );
            }
        }
        Ok((identities, peers))
    }

    fn capture_or_check(
        &mut self,
        pid: u32,
        registered: bool,
        fingerprint: Option<crate::process::ProcessBirthFingerprint>,
    ) -> Result<([Option<u32>; 2], bool)> {
        let (identities, peers) = self.inspect(pid, fingerprint)?;
        if self.initial.is_none() && !registered {
            for (index, identity) in identities.iter().enumerate() {
                anyhow::ensure!(
                    identity.is_none() || peers[index] == Some(pid),
                    "ACP runner socket owner is unknown: {}",
                    self.paths[index].display()
                );
            }
        }
        let control_present = identities[1].is_some();
        if self.initial.is_none() {
            for (index, identity) in identities.iter().enumerate() {
                if let Some(identity) = identity {
                    self.pins[index] = Some(crate::process::pin_socket_artifact(
                        &self.paths[index],
                        *identity,
                    )?);
                }
            }
            self.initial = Some(identities);
        }
        Ok((peers, control_present))
    }

    fn verify_cleanup(
        &self,
        pid: u32,
        fingerprint: Option<crate::process::ProcessBirthFingerprint>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.initial.is_some(),
            "ACP runner socket evidence was not captured"
        );
        self.inspect(pid, fingerprint)?;
        Ok(())
    }

    fn require_absent(&self) -> Result<()> {
        for path in &self.paths {
            anyhow::ensure!(
                socket_artifact_identity(path)?.is_none(),
                "ACP runner artifact has no confirmed process identity: {}",
                path.display()
            );
        }
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        for path in &self.paths {
            remove_file_if_present(path)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn signal_runner_if_owned(
    session_id: &str,
    expected_fence: Option<(&str, u64)>,
    expected_runner: Option<RunnerIdentity>,
    force_kill: bool,
    purge_artifacts: Option<&mut PurgeSocketEvidence>,
    guard: &mut Option<crate::process::RunnerProcessGuard>,
) -> Result<Option<RunnerIdentity>> {
    with_registry_lock(session_id, || {
        let mut fence = load_purge_fence_unlocked(session_id)?;
        match expected_fence {
            Some((profile, generation))
                if !fence.as_ref().is_some_and(|current| {
                    current.source_profile == profile && current.lifecycle_generation == generation
                }) =>
            {
                return Ok(None)
            }
            None if fence.is_some() => return Ok(None),
            _ => {}
        }
        let orphan = load_orphan_tracking_unlocked(session_id)?;
        let tracked = orphan.as_ref().map(|record| RunnerIdentity {
            pid: record.pid,
            generation: Some(record.generation),
            runner_identity: record
                .runner_identity
                .as_ref()
                .map(crate::process::ProcessBirthIdentity::fingerprint),
        });
        let record = load_strict_unlocked(session_id)?;
        let fallback_paths = if purge_artifacts.is_none() && record.is_none() {
            let base = socket_path_for(session_id)?;
            let control = crate::process::worker::control_socket_sibling(&base);
            Some([base, control])
        } else {
            None
        };
        let identity = if let Some(record) = record.as_ref() {
            RunnerIdentity {
                pid: record.pid,
                generation: Some(record.generation),
                runner_identity: record
                    .runner_identity
                    .as_ref()
                    .map(crate::process::ProcessBirthIdentity::fingerprint),
            }
        } else {
            let known = fence
                .as_ref()
                .and_then(|current| {
                    current.runner_pid.map(|pid| RunnerIdentity {
                        pid,
                        generation: current.runner_generation,
                        runner_identity: current
                            .runner_identity
                            .as_ref()
                            .map(crate::process::ProcessBirthIdentity::fingerprint),
                    })
                })
                .or(tracked);
            if expected_runner.is_some_and(|expected| known != Some(expected)) {
                return Ok(None);
            }
            if let Some(known) = known {
                known
            } else {
                let paths = purge_artifacts
                    .as_ref()
                    .map(|evidence| &evidence.paths)
                    .or(fallback_paths.as_ref())
                    .context("runner socket paths unavailable")?;
                let base_pid = crate::process::worker::peer_pid_from_socket(&paths[0]);
                let control_pid = crate::process::worker::peer_pid_from_socket(&paths[1]);
                anyhow::ensure!(
                    !matches!((base_pid, control_pid), (Some(left), Some(right)) if left != right),
                    "ACP runner socket identities disagree"
                );
                let Some(pid) = control_pid.or(base_pid) else {
                    return Ok(None);
                };
                RunnerIdentity {
                    pid,
                    generation: None,
                    runner_identity: crate::process::process_birth_identity(pid)?
                        .as_ref()
                        .map(crate::process::ProcessBirthIdentity::fingerprint),
                }
            }
        };
        if expected_runner.is_some_and(|expected| expected != identity) {
            return Ok(None);
        }
        anyhow::ensure!(
            identity.pid > 0 && identity.pid <= i32::MAX as u32,
            "ACP runner PID {} is invalid",
            identity.pid
        );
        if let Some(tracked) = tracked {
            anyhow::ensure!(
                identity.pid == tracked.pid
                    && identity
                        .generation
                        .is_none_or(|generation| Some(generation) == tracked.generation)
                    && identity.runner_identity == tracked.runner_identity,
                "ACP runner conflicts with orphan tracking evidence"
            );
        }
        if let Some(current) = fence.as_mut() {
            if let Some(pid) = current.runner_pid {
                let recorded = RunnerIdentity {
                    pid,
                    generation: current.runner_generation,
                    runner_identity: current
                        .runner_identity
                        .as_ref()
                        .map(crate::process::ProcessBirthIdentity::fingerprint),
                };
                anyhow::ensure!(
                    recorded == identity,
                    "ACP runner identity changed while purge fence was active"
                );
            } else {
                current.runner_pid = Some(identity.pid);
                current.runner_generation = identity.generation;
                let birth = crate::process::process_birth_identity(identity.pid)?;
                anyhow::ensure!(
                    birth
                        .as_ref()
                        .map(crate::process::ProcessBirthIdentity::fingerprint)
                        == identity.runner_identity,
                    "ACP runner birth changed before fencing"
                );
                current.runner_identity = birth;
                let path = purge_fence_path(session_id)?;
                let bytes =
                    serde_json::to_vec(current).context("serializing runner purge evidence")?;
                crate::session::atomic_write_verified(&path, &bytes)?;
                crate::session::sync_parent_directory(&path)?;
            }
        }
        if guard.is_none() {
            *guard = Some(crate::process::RunnerProcessGuard::capture(
                identity.pid,
                identity.runner_identity,
            )?);
        }
        let runner = guard.as_ref().expect("selected runner guard");
        let group_alive = runner.executing()?;
        if expected_fence.is_none() && expected_runner.is_some() && group_alive {
            if orphan.is_none() {
                write_orphan_tracking_unlocked(
                    record
                        .as_ref()
                        .context("orphan registry disappeared before tracking")?,
                )?;
            } else {
                crate::session::sync_parent_directory(&orphan_tracking_path(session_id)?)?;
            }
        }
        let peer = if let Some(evidence) = purge_artifacts {
            let (peers, control_present) = evidence.capture_or_check(
                identity.pid,
                record.is_some(),
                identity.runner_identity,
            )?;
            if let Some(record) = record.as_ref() {
                if record.socket_path == evidence.paths[0] {
                    peers[usize::from(control_present)]
                } else {
                    crate::process::worker::peer_pid_from_socket(&expected_socket(record))
                }
            } else {
                peers[1].or(peers[0])
            }
        } else if let Some(record) = record.as_ref() {
            crate::process::worker::peer_pid_from_socket(&expected_socket(record))
        } else {
            let paths = fallback_paths
                .as_ref()
                .context("orphan socket paths unavailable")?;
            let base_pid = crate::process::worker::peer_pid_from_socket(&paths[0]);
            let control_pid = crate::process::worker::peer_pid_from_socket(&paths[1]);
            anyhow::ensure!(
                !matches!((base_pid, control_pid), (Some(left), Some(right)) if left != right),
                "ACP runner socket identities disagree"
            );
            control_pid.or(base_pid)
        };
        match peer {
            Some(peer) if peer == identity.pid => {}
            Some(_) => {
                anyhow::bail!("ACP runner identity cannot be confirmed through its control socket")
            }
            None if !group_alive => return Ok(Some(identity)),
            None => {
                anyhow::bail!("ACP runner identity cannot be confirmed through its control socket")
            }
        }
        runner.signal(if force_kill {
            libc::SIGKILL
        } else {
            libc::SIGTERM
        })?;
        Ok(Some(identity))
    })
}

#[cfg(unix)]
async fn wait_for_runner_group_exit(runner: &crate::process::RunnerProcessGuard) -> Result<()> {
    use std::time::Duration;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while runner.executing()? && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_runner_group_exit_sync(runner: &crate::process::RunnerProcessGuard) -> Result<()> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    while runner.executing()? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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

/// Stop a fenced detached runner and prove its whole process group has exited.
pub fn terminate_and_confirm_stopped(
    session_id: &str,
    source_profile: &str,
    generation: u64,
) -> Result<()> {
    #[cfg(unix)]
    {
        let fence = Some((source_profile, generation));
        let cleanup_record = with_registry_lock(session_id, || {
            let current = load_strict_unlocked(session_id)?;
            let orphan = load_orphan_tracking_unlocked(session_id)?;
            if let (Some(current), Some(orphan)) = (&current, &orphan) {
                anyhow::ensure!(
                    same_runner(current, orphan),
                    "registry and orphan cleanup identities disagree"
                );
            }
            let persisted = load_purge_fence_unlocked(session_id)?
                .and_then(|fence| fence.runner_record)
                .map(serde_json::from_value::<WorkerRecord>)
                .transpose()?;
            if let (Some(current), Some(persisted)) =
                (current.as_ref().or(orphan.as_ref()), persisted.as_ref())
            {
                anyhow::ensure!(
                    same_runner(current, persisted),
                    "fenced native ownership changed"
                );
            }
            Ok(current.or(orphan).or(persisted))
        })?;
        let mut guard = None;
        let mut artifacts = PurgeSocketEvidence::new(session_id)?;
        let Some(identity) = signal_runner_if_owned(
            session_id,
            fence,
            None,
            false,
            Some(&mut artifacts),
            &mut guard,
        )?
        else {
            return with_registry_lock(session_id, || {
                anyhow::ensure!(
                    load_purge_fence_unlocked(session_id)?
                        .is_some_and(|current| current.source_profile == source_profile
                            && current.lifecycle_generation == generation),
                    "ACP purge fence changed before empty-runner confirmation"
                );
                anyhow::ensure!(
                    load_strict_unlocked(session_id)?.is_none(),
                    "ACP registry has no confirmed process identity"
                );
                artifacts.require_absent()
            });
        };
        #[cfg(test)]
        AFTER_FENCED_RUNNER_SIGNAL.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        wait_for_runner_group_exit_sync(guard.as_ref().context("runner guard missing")?)?;
        if guard
            .as_ref()
            .context("runner guard missing")?
            .executing()?
        {
            anyhow::ensure!(
                signal_runner_if_owned(
                    session_id,
                    fence,
                    Some(identity),
                    true,
                    Some(&mut artifacts),
                    &mut guard,
                )? == Some(identity),
                "ACP runner identity changed before SIGKILL escalation"
            );
            wait_for_runner_group_exit_sync(guard.as_ref().context("runner guard missing")?)?;
        }
        anyhow::ensure!(
            !guard
                .as_ref()
                .context("runner guard missing")?
                .executing()?,
            "ACP runner process group {} is still alive after SIGKILL",
            identity.pid
        );
        if let Some(record) = cleanup_record.as_ref() {
            let selected = RunnerIdentity {
                pid: record.pid,
                generation: Some(record.generation),
                runner_identity: record
                    .runner_identity
                    .as_ref()
                    .map(crate::process::ProcessBirthIdentity::fingerprint),
            };
            anyhow::ensure!(
                selected == identity,
                "native runtime ownership changed while terminating the fenced runner"
            );
            cleanup_runtime(record)?;
        }
        with_registry_lock(session_id, || {
            let current_fence = load_purge_fence_unlocked(session_id)?;
            anyhow::ensure!(
                current_fence.is_some_and(|current| current.source_profile == source_profile
                    && current.lifecycle_generation == generation),
                "ACP purge fence changed while terminating the runner"
            );
            anyhow::ensure!(
                !guard
                    .as_ref()
                    .context("runner guard missing")?
                    .executing()?,
                "ACP runner process group {} became live before registry cleanup",
                identity.pid
            );
            if let Some(current) = load_strict_unlocked(session_id)? {
                anyhow::ensure!(
                    identity.generation == Some(current.generation) && identity.pid == current.pid,
                    "ACP registry changed while terminating the fenced runner"
                );
            }
            let orphan = load_orphan_tracking_unlocked(session_id)?;
            if let Some(tracked) = orphan.as_ref() {
                anyhow::ensure!(
                    tracked.pid == identity.pid
                        && identity
                            .generation
                            .is_none_or(|generation| generation == tracked.generation),
                    "orphan tracking changed while terminating the fenced runner"
                );
            }
            artifacts.verify_cleanup(identity.pid, identity.runner_identity)?;
            anyhow::ensure!(
                !guard
                    .as_ref()
                    .context("runner guard missing")?
                    .executing()?,
                "ACP runner group became live before artifact cleanup"
            );
            artifacts.remove()?;
            let record = record_path(session_id)?;
            remove_file_if_present(&record)?;
            crate::session::sync_parent_directory(&record)?;
            if orphan.is_some() {
                remove_orphan_tracking_unlocked(session_id)?;
            }
            Ok(())
        })
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("cannot prove ACP runner process exit on this platform");
    }
}

#[cfg(unix)]
fn finalize_orphan_unlocked(record: &WorkerRecord, tracked: Option<&WorkerRecord>) -> Result<bool> {
    if let Some(tracked) = tracked {
        anyhow::ensure!(
            same_runner(tracked, record),
            "orphan tracking identity changed before cleanup"
        );
    }
    if load_purge_fence_unlocked(&record.session_id)?.is_some()
        || runner_group_alive(
            record.pid,
            record
                .runner_identity
                .as_ref()
                .map(crate::process::ProcessBirthIdentity::fingerprint),
        )
    {
        return Ok(false);
    }
    let current = load_strict_unlocked(&record.session_id)?;
    if current.as_ref().is_some_and(|current| {
        !same_runner(current, record) || current.socket_path != record.socket_path
    }) {
        if tracked.is_some() {
            remove_orphan_tracking_unlocked(&record.session_id)?;
        }
        return Ok(tracked.is_some());
    }
    let base = socket_path_for(&record.session_id)?;
    let control = crate::process::worker::control_socket_sibling(&base);
    let expected = expected_socket(record);
    let peers = [
        crate::process::worker::peer_pid_from_socket(&base),
        crate::process::worker::peer_pid_from_socket(&control),
        if expected != base && expected != control {
            crate::process::worker::peer_pid_from_socket(&expected)
        } else {
            None
        },
    ];
    let replacement_peer = if peers
        .iter()
        .any(|peer| peer.is_some_and(|pid| pid != record.pid))
    {
        true
    } else if peers.iter().any(Option::is_some) {
        let recorded = record
            .runner_identity
            .as_ref()
            .context("orphan peer birth identity is unknown")?;
        let actual = crate::process::process_birth_identity(record.pid)?
            .context("orphan peer disappeared during birth inspection")?;
        &actual != recorded
    } else {
        false
    };
    if replacement_peer {
        if tracked.is_some() {
            remove_orphan_tracking_unlocked(&record.session_id)?;
        }
        return Ok(tracked.is_some());
    }
    anyhow::ensure!(
        !runner_group_alive(
            record.pid,
            record
                .runner_identity
                .as_ref()
                .map(crate::process::ProcessBirthIdentity::fingerprint),
        ),
        "orphan group became live before cleanup"
    );
    if current.is_none() {
        anyhow::ensure!(
            !socket_exists(&base) && !socket_exists(&control),
            "unknown socket ownership prevents orphan artifact cleanup"
        );
    } else {
        remove_file_if_present(&base)?;
        remove_file_if_present(&control)?;
        remove_file_if_present(&record_path(&record.session_id)?)?;
    }
    if tracked.is_some() {
        remove_orphan_tracking_unlocked(&record.session_id)?;
    }
    Ok(true)
}

#[cfg(unix)]
pub(crate) enum OrphanTracking {
    Untracked,
    Pending,
    Reclaimable,
    Reclaimed,
}

#[cfg(unix)]
pub(crate) fn orphan_tracking_state(record: &WorkerRecord) -> Result<OrphanTracking> {
    with_registry_lock(&record.session_id, || {
        let tracked = load_orphan_tracking_unlocked(&record.session_id)?;
        let Some(tracked) = tracked
            .as_ref()
            .filter(|tracked| same_runner(tracked, record))
        else {
            return Ok(OrphanTracking::Untracked);
        };
        if runner_group_alive(
            tracked.pid,
            tracked
                .runner_identity
                .as_ref()
                .map(crate::process::ProcessBirthIdentity::fingerprint),
        ) {
            Ok(OrphanTracking::Pending)
        } else {
            Ok(OrphanTracking::Reclaimable)
        }
    })
}

#[cfg(unix)]
pub(crate) fn cleanup_dead_orphan_tracking(record: &WorkerRecord) -> Result<OrphanTracking> {
    match orphan_tracking_state(record)? {
        OrphanTracking::Untracked => return Ok(OrphanTracking::Untracked),
        OrphanTracking::Pending => return Ok(OrphanTracking::Pending),
        OrphanTracking::Reclaimable => {}
        OrphanTracking::Reclaimed => unreachable!("state inspection does not reclaim evidence"),
    }
    let tracked = with_registry_lock(&record.session_id, || {
        Ok(load_orphan_tracking_unlocked(&record.session_id)?
            .filter(|tracked| same_runner(tracked, record)))
    })?;
    let Some(tracked) = tracked else {
        return Ok(OrphanTracking::Untracked);
    };
    cleanup_runtime(&tracked)?;
    with_registry_lock(&record.session_id, || {
        let current = load_orphan_tracking_unlocked(&record.session_id)?;
        let Some(current) = current
            .as_ref()
            .filter(|current| same_runner(current, &tracked))
        else {
            return Ok(OrphanTracking::Untracked);
        };
        if finalize_orphan_unlocked(&tracked, Some(current))? {
            Ok(OrphanTracking::Reclaimed)
        } else {
            Ok(OrphanTracking::Pending)
        }
    })
}
#[cfg(unix)]
pub async fn terminate_orphan_and_confirm_stopped(record: &WorkerRecord) -> Result<bool> {
    let expected = RunnerIdentity {
        pid: record.pid,
        generation: Some(record.generation),
        runner_identity: record
            .runner_identity
            .as_ref()
            .map(crate::process::ProcessBirthIdentity::fingerprint),
    };
    let mut guard = None;
    let Some(identity) = signal_runner_if_owned(
        &record.session_id,
        None,
        Some(expected),
        false,
        None,
        &mut guard,
    )?
    else {
        return Ok(false);
    };
    wait_for_runner_group_exit(guard.as_ref().context("runner guard missing")?).await?;
    if guard
        .as_ref()
        .context("runner guard missing")?
        .executing()?
    {
        anyhow::ensure!(
            signal_runner_if_owned(
                &record.session_id,
                None,
                Some(identity),
                true,
                None,
                &mut guard
            )? == Some(identity),
            "ACP runner identity changed before SIGKILL escalation"
        );
        wait_for_runner_group_exit(guard.as_ref().context("runner guard missing")?).await?;
    }
    anyhow::ensure!(
        !guard
            .as_ref()
            .context("runner guard missing")?
            .executing()?,
        "ACP runner process group {} is still alive after SIGKILL",
        identity.pid
    );
    let runtime = record.clone();
    tokio::task::spawn_blocking(move || cleanup_runtime(&runtime)).await??;
    with_registry_lock(&record.session_id, || {
        let tracked = load_orphan_tracking_unlocked(&record.session_id)?;
        finalize_orphan_unlocked(record, tracked.as_ref())
    })
}

#[cfg(unix)]
fn runner_group_alive(
    pid: u32,
    fingerprint: Option<crate::process::ProcessBirthFingerprint>,
) -> bool {
    crate::process::RunnerProcessGuard::capture(pid, fingerprint)
        .and_then(|runner| runner.executing())
        .unwrap_or(true)
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
    with_registry_lock(&record.session_id, || {
        ensure_publication_unfenced(&record.session_id)?;
        anyhow::ensure!(
            load_strict_unlocked(&record.session_id)?
                .is_some_and(|current| same_runner(&current, &record)),
            "worker generation changed before termination"
        );
        Ok(())
    })?;
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
        let current = load_strict_unlocked(&selected.session_id)?;
        #[cfg(unix)]
        let orphan = load_orphan_tracking_unlocked(&selected.session_id)?;
        #[cfg(not(unix))]
        let orphan: Option<WorkerRecord> = None;
        if let (Some(current), Some(orphan)) = (&current, &orphan) {
            anyhow::ensure!(
                same_runner(current, orphan),
                "registry and orphan recovery identities disagree"
            );
        }
        let candidate = current
            .as_ref()
            .or(orphan.as_ref())
            .ok_or_else(|| anyhow::anyhow!("worker evidence disappeared before recovery"))?;
        anyhow::ensure!(
            same_runner(candidate, selected),
            "worker generation changed before recovery"
        );
        anyhow::ensure!(
            (candidate.agent_key == "prime-agent" || candidate.prime_runtime.is_some())
                && candidate
                    .prime_runtime
                    .as_ref()
                    .is_none_or(|runtime| runtime.proof_path.is_none()),
            "recovery is only for Prime records without a native ownership route"
        );
        anyhow::ensure!(
            candidate.runner_identity.as_ref().map_or_else(
                || Ok(!is_pid_alive(candidate.pid)),
                |expected| crate::process::runner_birth_identity(candidate.pid)
                    .map(|actual| actual.as_ref() != Some(expected))
            )?,
            "runner is still alive; recovery refused"
        );
        if current.is_some() {
            delete_unlocked(&selected.session_id)?;
        }
        #[cfg(unix)]
        if orphan.is_some() {
            remove_orphan_tracking_unlocked(&selected.session_id)?;
        }
        Ok(())
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
        assert!(load(id).unwrap().is_some());
        assert!(admit_resume(id, ResumeAuthority::Explicit, &Some(latest.clone())).is_err());
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
    fn rejected_publication_cannot_retire_an_unrecorded_socket_owner() {
        let root = TempDir::with_prefix_in("aoe-publish-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let id = "foreign-socket";
        let socket = socket_path_for(id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
        let selected = new_record(id, std::process::id(), socket);

        assert!(!retire_if_owned(&selected).unwrap());
        assert_eq!(
            crate::process::worker::peer_pid_from_socket(&control),
            Some(std::process::id())
        );
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
            assert!(load(id).is_err());
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

    #[tokio::test]
    #[cfg(target_os = "linux")]
    #[serial]
    async fn ordinary_stop_retires_the_route_only_after_the_full_runner_group_exits() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;

        struct OwnedGroup(std::process::Child);
        impl Drop for OwnedGroup {
            fn drop(&mut self) {
                crate::process::worker::kill_process_group(self.0.id());
                let _ = self.0.wait();
            }
        }

        let temp = TempDir::with_prefix_in("aoe-ordinary-stop-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let mut leader = OwnedGroup(
            std::process::Command::new("bash")
                .args(["-c", "trap '' TERM; sleep 600 & printf 'ready\\n'; wait"])
                .process_group(0)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let pid = leader.0.id();
        let mut ready = String::new();
        std::io::BufReader::new(leader.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");

        let record = new_record("ordinary-group-stop", pid, "/tmp/ordinary-group-stop.sock");
        save(&record).unwrap();
        terminate_and_wait("ordinary-group-stop").await.unwrap();

        assert!(!crate::process::runner_group_executing(pid).unwrap());
        assert!(load("ordinary-group-stop").unwrap().is_none());
    }

    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    struct KillProcessGroupOnDrop(u32);

    #[cfg(unix)]
    impl Drop for KillProcessGroupOnDrop {
        fn drop(&mut self) {
            crate::process::worker::kill_process_group(self.0);
        }
    }

    struct ReapedChild {
        pid: u32,
        kill: std::sync::mpsc::Sender<()>,
        exit: std::sync::mpsc::Receiver<std::io::Result<std::process::ExitStatus>>,
        finished: bool,
    }

    impl ReapedChild {
        fn new(mut child: std::process::Child) -> Self {
            use std::sync::mpsc;
            use std::time::Duration;

            let pid = child.id();
            let (kill_tx, kill_rx) = mpsc::channel();
            let (exit_tx, exit_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {}
                        Err(error) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break Err(error);
                        }
                    }

                    match kill_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = child.kill();
                            break child.wait();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                };
                let _ = exit_tx.send(result);
            });

            Self {
                pid,
                kill: kill_tx,
                exit: exit_rx,
                finished: false,
            }
        }

        fn id(&self) -> u32 {
            self.pid
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            let result = self.exit.recv().expect("runner waiter thread exited");
            self.finished = true;
            result
        }
    }

    impl Drop for ReapedChild {
        fn drop(&mut self) {
            if !self.finished {
                let _ = self.kill.send(());
                let _ = self.exit.recv();
            }
        }
    }

    fn with_temp_home<F: FnOnce()>(f: F) {
        // Keep worker socket paths below macOS sun_path limits.
        let tmp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        f();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    static CONTROL_RUNNER_TERMINATE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    extern "C" fn request_control_runner_termination(_: i32) {
        CONTROL_RUNNER_TERMINATE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn control_socket_runner_helper() {
        use std::os::unix::net::UnixListener;

        let Some(socket_path) = std::env::var_os("AOE_TEST_CONTROL_RUNNER_SOCKET") else {
            return;
        };
        let marker = std::env::var_os("AOE_TEST_CONTROL_RUNNER_MARKER");
        let listener = UnixListener::bind(socket_path).expect("bind helper control socket");
        let Some(marker) = marker else {
            return;
        };
        let action = if std::env::var_os("AOE_TEST_CONTROL_RUNNER_IGNORE_TERM").is_some() {
            nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::SigIgn,
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            )
        } else {
            nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::Handler(request_control_runner_termination),
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            )
        };
        unsafe {
            nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTERM, &action)
                .expect("install helper SIGTERM handler");
        }
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => drop(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept control probe: {error}"),
            }
            if CONTROL_RUNNER_TERMINATE.load(std::sync::atomic::Ordering::Relaxed) {
                std::fs::write(marker, b"SIGTERM").expect("record graceful shutdown");
                if let Some(session_id) = std::env::var_os("AOE_TEST_CONTROL_RUNNER_SELF_DELETE") {
                    let generation = std::env::var("AOE_TEST_CONTROL_RUNNER_GENERATION")
                        .expect("self-deleting runner generation")
                        .parse::<u64>()
                        .expect("valid self-deleting runner generation");
                    let session_id = session_id.to_string_lossy();
                    let record = load(&session_id).unwrap().unwrap();
                    assert_eq!(record.generation, generation);
                    assert!(delete_if_owned_by(
                        &session_id,
                        crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                    ));
                }
                if let Some(marker) = std::env::var_os("AOE_TEST_CONTROL_RUNNER_SURVIVING_CHILD") {
                    let output = std::process::Command::new("sh")
                        .args(["-c", "trap '' TERM; sleep 30 >/dev/null 2>&1 & echo $!"])
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        .expect("spawn process-group helper")
                        .wait_with_output()
                        .expect("reap process-group helper");
                    assert!(output.status.success(), "process-group helper failed");
                    let child_pid = String::from_utf8_lossy(&output.stdout);
                    std::fs::write(marker, child_pid.trim()).expect("record surviving child PID");
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    fn spawn_control_socket_runner(
        session_id: &str,
        ignore_term: bool,
        self_delete_generation: Option<u64>,
        surviving_child_marker: Option<&Path>,
    ) -> (ReapedChild, u32, PathBuf, PathBuf) {
        use std::os::unix::process::CommandExt as _;

        let base = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&base);
        let marker = workers_dir()
            .unwrap()
            .join(format!("{session_id}.term-marker"));
        let _ = std::fs::remove_file(&marker);
        let executable = std::env::current_exe().unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .args([
                "--exact",
                "process::worker_registry::tests::control_socket_runner_helper",
                "--nocapture",
            ])
            .env("AOE_TEST_CONTROL_RUNNER_SOCKET", &control)
            .env("AOE_TEST_CONTROL_RUNNER_MARKER", &marker)
            .process_group(0);
        if let Some(generation) = self_delete_generation {
            command
                .env("AOE_TEST_CONTROL_RUNNER_SELF_DELETE", session_id)
                .env("AOE_TEST_CONTROL_RUNNER_GENERATION", generation.to_string());
        }
        if let Some(marker) = surviving_child_marker {
            command.env("AOE_TEST_CONTROL_RUNNER_SURVIVING_CHILD", marker);
        }
        if ignore_term {
            command.env("AOE_TEST_CONTROL_RUNNER_IGNORE_TERM", "1");
        }
        let child = command.spawn().expect("spawn control-socket runner");
        let reaped_child = ReapedChild::new(child);
        let pid = reaped_child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if crate::process::worker::peer_pid_from_socket(&control) == Some(pid) {
                let mut probe = std::os::unix::net::UnixStream::connect(&control).unwrap();
                probe
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                assert_eq!(std::io::Read::read(&mut probe, &mut [0u8; 1]).unwrap(), 0);
                return (reaped_child, pid, control, marker);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("control socket did not report its runner PID {pid}");
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

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn terminating_without_a_record_preserves_unowned_sockets() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "untracked-route";
        let socket = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        std::fs::write(&socket, b"unowned socket path").unwrap();
        std::fs::write(&control, b"unowned control path").unwrap();

        terminate_and_wait(session_id).await.unwrap();

        assert_eq!(std::fs::read(&socket).unwrap(), b"unowned socket path");
        assert_eq!(std::fs::read(&control).unwrap(), b"unowned control path");
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
            assert!(update_stored_acp_session_id("sess-empty-acp", 1, "").is_err());
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
    fn retirement_preserves_replacement_record_and_socket() {
        with_temp_home(|| {
            let session_id = "replacement";
            let socket = socket_path_for(session_id).unwrap();
            touch_live_socket(&socket);
            let replacement_control = crate::process::worker::control_socket_sibling(&socket);
            let mut record = new_record(session_id, 111, socket);
            save(&record).unwrap();
            let selected = record.clone();
            record.pid = 222;
            save(&record).unwrap();

            assert!(!retire_if_owned(&selected).unwrap());
            assert_eq!(load(session_id).unwrap().unwrap().pid, 222);
            assert!(replacement_control.exists());
            assert!(retire_if_owned(&record).unwrap());
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

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_reaper_leaves_fenced_runner_for_purge_owner() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let (mut child, pid, _control, marker) =
            spawn_control_socket_runner("fenced-orphan", false, None, None);
        let record = new_record(
            "fenced-orphan",
            pid,
            socket_path_for("fenced-orphan").unwrap(),
        )
        .with_generation(9);
        save(&record).unwrap();
        fence_for_purge("fenced-orphan", "profile", 12).unwrap();

        assert!(!terminate_orphan_and_confirm_stopped(&record).await.unwrap());
        assert!(is_pid_alive(pid));
        assert_eq!(load("fenced-orphan").unwrap().unwrap().pid, pid);

        terminate_and_confirm_stopped("fenced-orphan", "profile", 12).unwrap();
        assert!(marker.exists(), "purge should send SIGTERM before cleanup");
        assert!(child.wait().unwrap().success());
        assert!(load("fenced-orphan").unwrap().is_none());
        assert!(clear_purge_fence_if_owned("fenced-orphan", "profile", 12).unwrap());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_does_not_signal_or_remove_artifacts_when_socket_pid_disagrees() {
        use std::os::unix::net::UnixListener;
        use std::os::unix::process::CommandExt as _;

        with_temp_home(|| {
            let session_id = "pid-reuse-protected";
            let child = KillOnDrop(
                std::process::Command::new("sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .expect("spawn unrelated process stand-in"),
            );
            let socket = socket_path_for(session_id).unwrap();
            let control = crate::process::worker::control_socket_sibling(&socket);
            let _foreign_listener = UnixListener::bind(&control).unwrap();
            let record = new_record(session_id, child.0.id(), socket);
            save(&record).unwrap();
            fence_for_purge(session_id, "profile", 3).unwrap();

            assert!(terminate_and_confirm_stopped(session_id, "profile", 3).is_err());
            assert!(is_pid_alive(child.0.id()));
            assert!(record_path(session_id).unwrap().exists());
            assert!(control.exists());
        });
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_reaper_preserves_artifacts_when_socket_pid_disagrees() {
        use std::os::unix::net::UnixListener;
        use std::os::unix::process::CommandExt as _;

        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "orphan-pid-reuse";
        let child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .expect("spawn unrelated process stand-in"),
        );
        let socket = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        let _foreign_listener = UnixListener::bind(&control).unwrap();
        let record = new_record(session_id, child.0.id(), socket).with_generation(7);
        save(&record).unwrap();

        assert!(terminate_orphan_and_confirm_stopped(&record).await.is_err());
        assert!(is_pid_alive(child.0.id()));
        assert!(record_path(session_id).unwrap().exists());
        assert!(control.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_uses_a_live_socket_pid_when_the_registry_record_is_absent() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "socket-fallback";
        let (mut child, _pid, control, marker) =
            spawn_control_socket_runner(session_id, false, None, None);
        fence_for_purge(session_id, "profile", 4).unwrap();

        terminate_and_confirm_stopped(session_id, "profile", 4).unwrap();

        assert!(marker.exists());
        assert!(child.wait().unwrap().success());
        assert!(!record_path(session_id).unwrap().exists());
        assert!(!control.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_completes_when_recorded_runner_has_exited_and_socket_remains() {
        use std::os::unix::net::UnixListener;

        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "dead-runner-artifacts";
        let socket = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        let stale_listener = UnixListener::bind(&control).unwrap();
        drop(stale_listener);
        let record = new_record(session_id, 2_000_000_000, socket.clone()).with_generation(8);
        save(&record).unwrap();
        fence_for_purge(session_id, "profile", 9).unwrap();

        terminate_and_confirm_stopped(session_id, "profile", 9).unwrap();

        assert!(load(session_id).unwrap().is_none());
        assert!(!socket_exists(&socket));
        assert!(!socket_exists(&control));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_retains_unknown_sockets_with_a_preexisting_empty_fence() {
        use std::os::unix::net::UnixListener;

        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "stale-sockets-no-record";
        let socket = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        fence_for_purge(session_id, "profile", 10).unwrap();
        let stale_listener = UnixListener::bind(&control).unwrap();
        drop(stale_listener);

        terminate_and_confirm_stopped(session_id, "profile", 10)
            .expect_err("an unknown socket cannot prove group exit");

        assert!(!record_path(session_id).unwrap().exists());
        assert!(!socket_exists(&socket));
        assert!(socket_exists(&control));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_preserves_live_runner_when_its_socket_is_missing() {
        use std::os::unix::process::CommandExt as _;

        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "live-runner-missing-socket";
        let child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .expect("spawn runner stand-in"),
        );
        let socket = socket_path_for(session_id).unwrap();
        let record = new_record(session_id, child.0.id(), socket).with_generation(11);
        save(&record).unwrap();
        fence_for_purge(session_id, "profile", 12).unwrap();

        assert!(terminate_and_confirm_stopped(session_id, "profile", 12).is_err());
        assert!(is_pid_alive(child.0.id()));
        assert!(record_path(session_id).unwrap().exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_preserves_process_group_when_runner_leader_has_exited() {
        use std::os::unix::net::UnixListener;
        use std::os::unix::process::CommandExt as _;
        use std::process::Stdio;

        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "exit-leader-child";
        let leader = std::process::Command::new("sh")
            .args(["-c", "sleep 60 >/dev/null 2>&1 & printf '%s\\n' \"$!\""])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn runner leader stand-in");
        let leader_pid = leader.id();
        let _kill_group = KillProcessGroupOnDrop(leader_pid);
        let child_pid = String::from_utf8(leader.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .parse::<u32>()
            .expect("read surviving child PID");
        assert!(!is_pid_alive(leader_pid));
        assert!(is_pid_alive(child_pid));
        assert!(crate::process::worker::is_process_group_alive(leader_pid));

        let socket = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&socket);
        let stale_listener = UnixListener::bind(&control).unwrap();
        drop(stale_listener);
        let record = new_record(session_id, leader_pid, socket).with_generation(13);
        save(&record).unwrap();
        fence_for_purge(session_id, "profile", 14).unwrap();

        assert!(terminate_and_confirm_stopped(session_id, "profile", 14).is_err());
        assert!(is_pid_alive(child_pid));
        assert!(record_path(session_id).unwrap().exists());
        assert!(control.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_rechecks_runner_identity_before_sigkill_escalation() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "kill-escalation";
        let (mut child, pid, _control, marker) =
            spawn_control_socket_runner(session_id, true, None, None);
        let record =
            new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5);
        save(&record).unwrap();
        fence_for_purge(session_id, "profile", 6).unwrap();

        terminate_and_confirm_stopped(session_id, "profile", 6).unwrap();

        assert!(!marker.exists(), "SIGKILL follows the SIGTERM grace period");
        assert!(!child.wait().unwrap().success());
        assert!(load(session_id).unwrap().is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_succeeds_when_runner_removes_its_record_before_exit() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "self-deleting-runner";
        let generation = 15;
        let (mut child, pid, _control, marker) =
            spawn_control_socket_runner(session_id, false, Some(3), None);
        save(&new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(3))
            .unwrap();
        fence_for_purge(session_id, "profile", generation).unwrap();

        terminate_and_confirm_stopped(session_id, "profile", generation).unwrap();

        assert!(marker.exists());
        assert!(child.wait().unwrap().success());
        assert!(load(session_id).unwrap().is_none());
        assert!(!socket_exists(&socket_path_for(session_id).unwrap()));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_preserves_fenced_identity_after_runner_self_deletes_with_live_descendant() {
        let temp = TempDir::with_prefix_in("aoe-registry-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "self-child";
        let generation = 16;
        let child_marker = workers_dir().unwrap().join("child.pid");
        let (mut child, pid, _control, marker) =
            spawn_control_socket_runner(session_id, false, Some(generation), Some(&child_marker));
        let _kill_group = KillProcessGroupOnDrop(pid);
        save(
            &new_record(session_id, pid, socket_path_for(session_id).unwrap())
                .with_generation(generation),
        )
        .unwrap();
        fence_for_purge(session_id, "profile", generation).unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
        std::fs::write(workspace.join("writer-data"), b"must survive").unwrap();
        let storage = crate::session::Storage::new_unwatched("profile").unwrap();
        let mut instance = crate::session::Instance::new("survivor", workspace.to_str().unwrap());
        instance.id = session_id.to_string();
        instance.source_profile = "profile".to_string();
        instance.view = crate::session::View::Structured;
        instance.scratch = true;
        instance.status = crate::session::Status::Deleting;
        instance.lifecycle_generation = generation;
        instance.lifecycle_reservation = Some(crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Purge,
            generation,
            at: chrono::Utc::now()
                - crate::session::Instance::LIFECYCLE_RESERVATION_TTL
                - chrono::Duration::seconds(1),
        });
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            instance,
            crate::session::Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        )
        .with_phase(crate::session::lifecycle_journal::LifecyclePhase::TeardownStarted);
        crate::session::lifecycle_journal::record(&entry).unwrap();

        terminate_and_confirm_stopped(session_id, "profile", generation)
            .expect_err("a surviving descendant must keep the fenced identity live");
        assert!(marker.exists());
        assert!(!record_path(session_id).unwrap().exists());
        assert!(!socket_exists(&socket_path_for(session_id).unwrap()));
        let child_pid = std::fs::read_to_string(&child_marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(is_pid_alive(child_pid));
        assert!(crate::process::worker::is_process_group_alive(pid));

        fence_for_purge(session_id, "profile", generation + 1)
            .expect("recovery must renew the purge fence");
        terminate_and_confirm_stopped(session_id, "profile", generation + 1)
            .expect_err("a retry must not treat missing artifacts as an exited group");
        let fence = load_purge_fence_unlocked(session_id).unwrap().unwrap();
        assert_eq!(fence.runner_pid, Some(pid));
        assert_eq!(fence.lifecycle_generation, generation + 1);
        assert_eq!(fence.runner_generation, Some(generation));
        assert!(is_pid_alive(child_pid));
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(
            workspace.join("writer-data").exists(),
            "recovery removed a live writer workspace"
        );
        let rows = storage.load().unwrap();
        assert_eq!(rows[0].status, crate::session::Status::Deleting);
        assert!(is_pid_alive(child_pid));
        let retry_generation = load_purge_fence_unlocked(session_id)
            .unwrap()
            .unwrap()
            .lifecycle_generation;

        let fingerprint = fence
            .runner_identity
            .as_ref()
            .map(crate::process::ProcessBirthIdentity::fingerprint);
        crate::process::worker::kill_process_group(pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runner_group_alive(pid, fingerprint) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!runner_group_alive(pid, fingerprint));
        terminate_and_confirm_stopped(session_id, "profile", retry_generation).unwrap();
        assert!(!record_path(session_id).unwrap().exists());
        assert!(!socket_exists(&socket_path_for(session_id).unwrap()));
        assert!(clear_purge_fence_if_owned(session_id, "profile", retry_generation).unwrap());
        assert!(child.wait().is_ok());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_reaper_keeps_surviving_group_discoverable_after_runner_self_cleanup() {
        let temp = TempDir::with_prefix_in("aoe-orphan-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "orphan-self-child";
        let generation = 5;
        let child_marker = workers_dir().unwrap().join("orphan-child.pid");
        let (mut child, pid, _, _) =
            spawn_control_socket_runner(session_id, false, Some(generation), Some(&child_marker));
        let _kill_group = KillProcessGroupOnDrop(pid);
        let record = new_record(session_id, pid, socket_path_for(session_id).unwrap())
            .with_generation(generation);
        save(&record).unwrap();
        assert!(terminate_orphan_and_confirm_stopped(&record).await.is_err());
        assert!(child.wait().unwrap().success());
        let child_pid = std::fs::read_to_string(child_marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(is_pid_alive(child_pid));
        assert!(crate::process::worker::is_process_group_alive(pid));
        assert!(
            list_for_reaping()
                .unwrap()
                .iter()
                .any(|candidate| candidate.session_id == session_id),
            "self-cleanup lost a live orphan group from future sweeps"
        );
        assert!(terminate_orphan_and_confirm_stopped(&record).await.is_err());
        assert!(list_for_reaping()
            .unwrap()
            .iter()
            .any(|candidate| same_runner(candidate, &record)));
        crate::process::worker::kill_process_group(pid);
        wait_for_runner_group_exit(
            &crate::process::RunnerProcessGuard::capture(pid, None).unwrap(),
        )
        .await
        .unwrap();
        assert!(!runner_group_alive(pid, None));
        assert!(terminate_orphan_and_confirm_stopped(&record).await.unwrap());
        assert!(!orphan_tracking_path(session_id).unwrap().exists());
        assert!(!list_for_reaping()
            .unwrap()
            .iter()
            .any(|candidate| candidate.session_id == session_id));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn purge_refuses_an_existing_socket_with_unknown_owner() {
        use crate::session::deletion::{DeletionRequest, PurgeTransaction};
        let temp = TempDir::with_prefix_in("aoe-unknown-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "unknown-owner";
        let base = socket_path_for(session_id).unwrap();
        let control = crate::process::worker::control_socket_sibling(&base);
        let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
        drop(listener);
        assert!(socket_exists(&control));
        assert_eq!(crate::process::worker::peer_pid_from_socket(&control), None);
        let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
        std::fs::write(
            workspace.join("retained"),
            b"unknown writer must not be assumed absent",
        )
        .unwrap();
        let storage = crate::session::Storage::new_unwatched("profile").unwrap();
        let mut instance = crate::session::Instance::new("unknown", workspace.to_str().unwrap());
        instance.id = session_id.to_string();
        instance.source_profile = "profile".into();
        instance.view = crate::session::View::Structured;
        instance.scratch = true;
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let result = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                session_id: session_id.into(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            },
        );
        assert!(result.is_err(), "purge accepted an unknown socket owner");
        assert!(socket_exists(&control));
        assert!(workspace.join("retained").exists());
        let row = storage.load().unwrap().remove(0);
        assert_eq!(row.status, crate::session::Status::Idle);
        assert!(row.lifecycle_reservation.is_none());
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn aborted_purge_keeps_the_last_fence_until_a_self_cleaned_group_stops() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        let temp = TempDir::with_prefix_in("aoe-abort-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let id = "abort-self-child";
        let child_marker = workers_dir().unwrap().join("child.pid");
        let (mut child, pid, _, _) =
            spawn_control_socket_runner(id, false, Some(5), Some(&child_marker));
        let _kill_group = KillProcessGroupOnDrop(pid);
        let record = new_record(id, pid, socket_path_for(id).unwrap()).with_generation(5);
        save(&record).unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(id).unwrap();
        std::fs::write(workspace.join("retained"), b"aborted purge keeps scratch").unwrap();
        let storage = crate::session::Storage::new_unwatched("profile").unwrap();
        let mut instance = crate::session::Instance::new("abort", workspace.to_str().unwrap());
        instance.id = id.into();
        instance.source_profile = "profile".into();
        instance.view = crate::session::View::Structured;
        instance.scratch = true;
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let PurgeReservation::Reserved(transaction) = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                session_id: id.into(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            },
        )
        .unwrap() else {
            panic!("purge was rejected");
        };
        let transaction = transaction.run_hooks();
        let fence = with_registry_lock(id, || load_purge_fence_unlocked(id))
            .unwrap()
            .unwrap();
        let generation = fence.lifecycle_generation;
        assert_eq!(fence.runner_pid, Some(pid));
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(is_pid_alive(
            std::fs::read_to_string(&child_marker)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        ));
        assert!(load(id).unwrap().is_none());
        assert!(!orphan_tracking_path(id).unwrap().exists());
        drop(transaction);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let owner_released = storage.try_acquire_purge_owner_lock(id).unwrap().is_some();
            let row = storage.load().unwrap().remove(0);
            if owner_released && row.lifecycle_reservation.is_none() {
                assert_eq!(row.status, crate::session::Status::Idle);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "abort did not settle");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let path =
            crate::session::lifecycle_journal::path_for(storage.sessions_path(), id, generation);
        assert_eq!(
            crate::session::lifecycle_journal::read_deletion(&path)
                .unwrap()
                .unwrap()
                .phase,
            crate::session::lifecycle_journal::LifecyclePhase::Abandoned
        );
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(path.exists());
        assert!(is_purge_fenced(id).unwrap());
        assert!(workspace.join("retained").exists());
        crate::process::worker::kill_process_group(pid);
        while runner_group_alive(
            pid,
            record
                .runner_identity
                .as_ref()
                .map(crate::process::ProcessBirthIdentity::fingerprint),
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "owned group did not stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(!path.exists());
        assert!(!is_purge_fenced(id).unwrap());
        assert!(!orphan_tracking_path(id).unwrap().exists());
        assert_eq!(
            std::fs::read(workspace.join("retained")).unwrap(),
            b"aborted purge keeps scratch"
        );
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn marker_only_group_blocks_purge_until_death_and_preserves_a_successor() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        for successor_present in [false, true] {
            let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let session_id = "takeover";
            let child_marker = workers_dir().unwrap().join("child.pid");
            let (mut child, pid, control, _) =
                spawn_control_socket_runner(session_id, false, Some(5), Some(&child_marker));
            let _kill_group = KillProcessGroupOnDrop(pid);
            let old = new_record(session_id, pid, socket_path_for(session_id).unwrap())
                .with_generation(5);
            save(&old).unwrap();
            assert!(terminate_orphan_and_confirm_stopped(&old).await.is_err());
            assert!(child.wait().unwrap().success());
            assert!(load(session_id).unwrap().is_none());
            assert!(runner_group_alive(pid, None));
            let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
            std::fs::write(workspace.join("retained"), b"tracked writer").unwrap();
            let storage = crate::session::Storage::new_unwatched("profile").unwrap();
            let mut instance =
                crate::session::Instance::new("takeover", workspace.to_str().unwrap());
            instance.id = session_id.into();
            instance.source_profile = "profile".into();
            instance.view = crate::session::View::Structured;
            instance.scratch = true;
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let successor = new_record(
                session_id,
                std::process::id(),
                socket_path_for(session_id).unwrap(),
            )
            .with_generation(6);
            let listener = successor_present.then(|| publish_runner(&successor).unwrap());
            let result = PurgeTransaction::reserve(
                storage.clone(),
                DeletionRequest {
                    session_id: session_id.into(),
                    instance,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                },
            );
            if successor_present {
                assert!(result.is_err());
                assert!(workspace.join("retained").exists());
                assert_eq!(
                    storage.load().unwrap()[0].status,
                    crate::session::Status::Idle
                );
                assert_eq!(
                    crate::process::worker::peer_pid_from_socket(&control),
                    Some(std::process::id())
                );
            } else {
                let transaction = match result.unwrap() {
                    PurgeReservation::Reserved(transaction) => transaction,
                    PurgeReservation::Rejected(result) => {
                        panic!("unexpected rejection: {result:?}")
                    }
                };
                assert!(
                    !transaction
                        .run_hooks()
                        .begin_irreversible()
                        .unwrap()
                        .finish()
                        .success
                );
                assert!(workspace.join("retained").exists());
                assert_eq!(
                    storage.load().unwrap()[0].status,
                    crate::session::Status::Deleting
                );
            }
            assert!(orphan_tracking_path(session_id).unwrap().exists());
            assert!(runner_group_alive(pid, None));
            crate::process::worker::kill_process_group(pid);
            wait_for_runner_group_exit(
                &crate::process::RunnerProcessGuard::capture(pid, None).unwrap(),
            )
            .await
            .unwrap();
            assert!(!runner_group_alive(pid, None));
            if successor_present {
                assert!(matches!(
                    cleanup_dead_orphan_tracking(&old).unwrap(),
                    OrphanTracking::Reclaimed
                ));
                assert!(same_runner(&load(session_id).unwrap().unwrap(), &successor));
                assert_eq!(
                    crate::process::worker::peer_pid_from_socket(&control),
                    Some(std::process::id())
                );
                assert!(workspace.join("retained").exists());
            } else {
                storage
                    .update(|rows, _| {
                        rows[0].lifecycle_reservation.as_mut().unwrap().at =
                            chrono::Utc::now() - chrono::Duration::days(1);
                        Ok(())
                    })
                    .unwrap();
                crate::session::deletion::recover_lifecycle_journals_once().unwrap();
                assert!(!workspace.exists());
                assert!(storage.load().unwrap().is_empty());
                assert!(crate::session::lifecycle_journal::scan([storage
                    .sessions_path()
                    .to_path_buf()])
                .entries
                .is_empty());
                assert!(!is_purge_fenced(session_id).unwrap());
            }
            assert!(!orphan_tracking_path(session_id).unwrap().exists());
            drop(listener);
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_cleanup_retains_evidence_on_errors_and_preserves_successors() {
        enum Case {
            CleanupFailure,
            UnknownArtifact,
            SuccessorRecord,
            SuccessorPeer,
        }
        for case in [
            Case::CleanupFailure,
            Case::UnknownArtifact,
            Case::SuccessorRecord,
            Case::SuccessorPeer,
        ] {
            let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let session_id = "tracked";
            let (mut child, pid, control, _) =
                spawn_control_socket_runner(session_id, false, None, None);
            let _kill_group = KillProcessGroupOnDrop(pid);
            let old = new_record(session_id, pid, socket_path_for(session_id).unwrap())
                .with_generation(5);
            save(&old).unwrap();
            with_registry_lock(session_id, || write_orphan_tracking_unlocked(&old)).unwrap();
            crate::process::worker::kill_process_group(pid);
            child.wait().unwrap();
            assert!(!runner_group_alive(pid, None));
            match case {
                Case::CleanupFailure => {
                    std::fs::remove_file(&control).unwrap();
                    std::fs::create_dir(&control).unwrap();
                    assert!(cleanup_dead_orphan_tracking(&old).is_err());
                    assert!(load(session_id).unwrap().is_some());
                    assert!(orphan_tracking_path(session_id).unwrap().exists());
                    std::fs::remove_dir(&control).unwrap();
                    assert!(matches!(
                        cleanup_dead_orphan_tracking(&old).unwrap(),
                        OrphanTracking::Reclaimed
                    ));
                    assert!(load(session_id).unwrap().is_none());
                    assert!(!orphan_tracking_path(session_id).unwrap().exists());
                }
                Case::UnknownArtifact => {
                    std::fs::remove_file(record_path(session_id).unwrap()).unwrap();
                    assert!(cleanup_dead_orphan_tracking(&old).is_err());
                    assert!(socket_exists(&control));
                    assert!(orphan_tracking_path(session_id).unwrap().exists());
                    std::fs::remove_file(&control).unwrap();
                    assert!(matches!(
                        cleanup_dead_orphan_tracking(&old).unwrap(),
                        OrphanTracking::Reclaimed
                    ));
                    assert!(!orphan_tracking_path(session_id).unwrap().exists());
                }
                Case::SuccessorRecord | Case::SuccessorPeer => {
                    let successor = new_record(
                        session_id,
                        std::process::id(),
                        socket_path_for(session_id).unwrap(),
                    )
                    .with_generation(6);
                    retire_selected(&old).unwrap();
                    let listener = publish_runner(&successor).unwrap();
                    if matches!(case, Case::SuccessorPeer) {
                        save(&old).unwrap();
                    }
                    assert!(matches!(
                        cleanup_dead_orphan_tracking(&old).unwrap(),
                        OrphanTracking::Reclaimed
                    ));
                    assert!(!orphan_tracking_path(session_id).unwrap().exists());
                    assert!(load(session_id).unwrap().is_some());
                    assert_eq!(
                        crate::process::worker::peer_pid_from_socket(&control),
                        Some(std::process::id())
                    );
                    if matches!(case, Case::SuccessorRecord) {
                        assert_eq!(load(session_id).unwrap().unwrap().generation, 6);
                    }
                    drop(listener);
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn publication_preserves_the_existing_live_listener() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "publish";
        let (mut child, pid, control, _) =
            spawn_control_socket_runner(session_id, false, None, None);
        let _kill_group = KillProcessGroupOnDrop(pid);
        let old =
            new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5);
        save(&old).unwrap();
        let successor = new_record(
            session_id,
            std::process::id(),
            socket_path_for(session_id).unwrap(),
        )
        .with_generation(6);
        assert!(publish_runner(&successor).is_err());
        assert_eq!(
            crate::process::worker::peer_pid_from_socket(&control),
            Some(pid)
        );
        assert!(same_runner(&load(session_id).unwrap().unwrap(), &old));
        assert!(is_pid_alive(pid));
        crate::process::worker::kill_process_group(pid);
        child.wait().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn purge_fence_rejects_publication_without_a_prior_owner() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "fenced-empty";
        fence_for_purge(session_id, "profile", 9).unwrap();
        let record = new_record(
            session_id,
            std::process::id(),
            socket_path_for(session_id).unwrap(),
        )
        .with_generation(1);

        assert!(publish_runner(&record).is_err());
        assert!(load(session_id).unwrap().is_none());
        assert!(
            !crate::process::worker::control_socket_sibling(&record.socket_path)
                .try_exists()
                .unwrap()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_tracking_failure_prevents_graceful_shutdown() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "writefail";
        let (mut child, pid, control, termination_marker) =
            spawn_control_socket_runner(session_id, false, None, None);
        let _kill_group = KillProcessGroupOnDrop(pid);
        let old =
            new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5);
        save(&old).unwrap();
        std::fs::create_dir(orphan_tracking_path(session_id).unwrap()).unwrap();
        assert!(terminate_orphan_and_confirm_stopped(&old).await.is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            assert!(!termination_marker.exists());
            assert!(is_pid_alive(pid));
            assert_eq!(
                crate::process::worker::peer_pid_from_socket(&control),
                Some(pid)
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(load(session_id).unwrap().is_some());
        crate::process::worker::kill_process_group(pid);
        child.wait().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[tokio::test]
    #[serial]
    async fn orphan_reaper_retains_prime_ownership_without_a_native_route() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "prime-orphan";
        let (mut child, pid, _, _) = spawn_control_socket_runner(session_id, false, None, None);
        let _kill_group = KillProcessGroupOnDrop(pid);
        let mut record =
            new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5);
        record.agent_key = "prime-agent".into();
        save(&record).unwrap();
        with_registry_lock(session_id, || write_orphan_tracking_unlocked(&record)).unwrap();
        crate::process::worker::kill_process_group(pid);
        child.wait().unwrap();

        assert!(matches!(
            orphan_tracking_state(&record).unwrap(),
            OrphanTracking::Reclaimable
        ));
        assert!(terminate_orphan_and_confirm_stopped(&record).await.is_err());
        assert!(cleanup_dead_orphan_tracking(&record).is_err());
        assert!(same_runner(&load(session_id).unwrap().unwrap(), &record));
        assert!(orphan_tracking_path(session_id).unwrap().exists());

        fence_for_purge(session_id, "profile", 9).unwrap();
        assert!(terminate_and_confirm_stopped(session_id, "profile", 9).is_err());
        assert!(same_runner(&load(session_id).unwrap().unwrap(), &record));
        assert!(orphan_tracking_path(session_id).unwrap().exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn confirmed_prime_recovery_retires_orphan_only_evidence() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "prime-orphan-only";
        let mut child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let mut record = new_record(
            session_id,
            child.0.id(),
            socket_path_for(session_id).unwrap(),
        )
        .with_generation(7);
        record.agent_key = "prime-agent".into();
        save(&record).unwrap();
        with_registry_lock(session_id, || write_orphan_tracking_unlocked(&record)).unwrap();
        std::fs::remove_file(record_path(session_id).unwrap()).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();

        let selected = load_recovery_candidate(session_id).unwrap().unwrap();
        assert!(same_runner(&selected, &record));
        recover_untracked_prime(&selected).unwrap();
        assert!(load_recovery_candidate(session_id).unwrap().is_none());
        assert!(!orphan_tracking_path(session_id).unwrap().exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn confirmed_worker_recovery_retires_orphan_only_evidence() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "worker-orphan-only";
        let mut child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let record = new_record(
            session_id,
            child.0.id(),
            socket_path_for(session_id).unwrap(),
        )
        .with_generation(8);
        save(&record).unwrap();
        with_registry_lock(session_id, || write_orphan_tracking_unlocked(&record)).unwrap();
        std::fs::remove_file(record_path(session_id).unwrap()).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();

        recover_worker(session_id).unwrap();
        assert!(load_recovery_candidate(session_id).unwrap().is_none());
        assert!(!orphan_tracking_path(session_id).unwrap().exists());
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn committed_purge_retains_resources_when_an_unknown_socket_appears() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        for captured_runner in [false, true] {
            let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let session_id = "late-socket";
            let (mut runner, _kill_group) = if captured_runner {
                let (child, pid, _, _) =
                    spawn_control_socket_runner(session_id, false, Some(5), None);
                let guard = KillProcessGroupOnDrop(pid);
                save(
                    &new_record(session_id, pid, socket_path_for(session_id).unwrap())
                        .with_generation(5),
                )
                .unwrap();
                (Some(child), Some(guard))
            } else {
                (None, None)
            };
            let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
            std::fs::write(workspace.join("retained"), b"retain unknown ownership").unwrap();
            let storage = crate::session::Storage::new_unwatched("profile").unwrap();
            let mut instance = crate::session::Instance::new("late", workspace.to_str().unwrap());
            instance.id = session_id.into();
            instance.source_profile = "profile".into();
            instance.view = crate::session::View::Structured;
            instance.scratch = true;
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let transaction = match PurgeTransaction::reserve(
                storage.clone(),
                DeletionRequest {
                    session_id: session_id.into(),
                    instance,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
            };
            let committed = transaction.run_hooks().begin_irreversible().unwrap();
            if let Some(child) = runner.as_mut() {
                crate::process::worker::terminate_process_group(child.id());
                assert!(child.wait().unwrap().success());
                assert!(!runner_group_alive(child.id(), None));
                assert!(load(session_id).unwrap().is_none());
            }
            let control = crate::process::worker::control_socket_sibling(
                &socket_path_for(session_id).unwrap(),
            );
            let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
            drop(listener);
            assert!(socket_exists(&control));
            assert_eq!(crate::process::worker::peer_pid_from_socket(&control), None);
            let result = committed.finish();
            assert!(
                !result.success,
                "unknown socket accepted; socket={}, workspace={}",
                socket_exists(&control),
                workspace.exists()
            );
            assert!(socket_exists(&control));
            assert!(workspace.join("retained").exists());
            assert_eq!(
                storage.load().unwrap()[0].status,
                crate::session::Status::Deleting
            );
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_keeps_resources_when_external_shutdown_precedes_registry_stop() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        for published_record in [true, false] {
            let temp = TempDir::with_prefix_in("aoe-pre-stop-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let session_id = "pre-stop-child";
            let runner_generation = 5;
            let child_marker = workers_dir().unwrap().join("pre-stop-child.pid");
            let (mut child, pid, control, _) = spawn_control_socket_runner(
                session_id,
                false,
                published_record.then_some(runner_generation),
                Some(&child_marker),
            );
            let _kill_group = KillProcessGroupOnDrop(pid);
            if published_record {
                save(
                    &new_record(session_id, pid, socket_path_for(session_id).unwrap())
                        .with_generation(runner_generation),
                )
                .unwrap();
            }
            let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
            std::fs::write(workspace.join("writer-data"), b"must survive").unwrap();
            let storage = crate::session::Storage::new_unwatched("profile").unwrap();
            let mut instance =
                crate::session::Instance::new("pre-stop", workspace.to_str().unwrap());
            instance.id = session_id.to_string();
            instance.source_profile = "profile".to_string();
            instance.view = crate::session::View::Structured;
            instance.scratch = true;
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
            let _events = crate::acp::event_store::EventStore::open(&db_path, 100).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at) VALUES (?1, 0, '{}', 0), ('witness', 0, '{}', 0)",
                [session_id],
            ).unwrap();
            let transaction = match PurgeTransaction::reserve_with_acp_transcript(
                storage.clone(),
                DeletionRequest {
                    session_id: session_id.to_string(),
                    instance,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(result) => panic!("purge rejected: {result:?}"),
            };
            let committed = transaction.run_hooks().begin_irreversible().unwrap();
            crate::process::worker::terminate_process_group(pid);
            assert!(child.wait().unwrap().success());
            let child_pid = std::fs::read_to_string(child_marker)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
            assert!(!record_path(session_id).unwrap().exists());
            assert!(is_pid_alive(child_pid));
            assert!(crate::process::worker::is_process_group_alive(pid));
            let result = committed.finish();
            assert!(
                workspace.join("writer-data").exists(),
                "pre-cleanup shutdown lost the live writer workspace"
            );
            assert!(!result.success);
            assert_eq!(
                storage.load().unwrap()[0].status,
                crate::session::Status::Deleting
            );
            assert!(is_pid_alive(child_pid));
            let initial = load_purge_fence_unlocked(session_id).unwrap().unwrap();
            for _ in 0..2 {
                storage
                    .update(|rows, _| {
                        rows[0].lifecycle_reservation.as_mut().unwrap().at =
                            chrono::Utc::now() - chrono::Duration::days(1);
                        Ok(())
                    })
                    .unwrap();
                crate::session::deletion::recover_lifecycle_journals_once().unwrap();
                let fence = load_purge_fence_unlocked(session_id).unwrap().unwrap();
                assert_eq!(fence.runner_pid, Some(pid));
                assert_eq!(
                    fence.runner_generation,
                    published_record.then_some(runner_generation)
                );
                assert!(workspace.join("writer-data").exists());
                assert_eq!(
                    conn.query_row(
                        "SELECT count(*) FROM acp_events WHERE session_id = ?1",
                        [session_id],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                    1
                );
                assert!(crate::process::worker::is_process_group_alive(pid));
            }
            crate::process::worker::kill_process_group(pid);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while runner_group_alive(
                pid,
                initial
                    .runner_identity
                    .as_ref()
                    .map(crate::process::ProcessBirthIdentity::fingerprint),
            ) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            storage
                .update(|rows, _| {
                    rows[0].lifecycle_reservation.as_mut().unwrap().at =
                        chrono::Utc::now() - chrono::Duration::days(1);
                    Ok(())
                })
                .unwrap();
            crate::session::deletion::recover_lifecycle_journals_once().unwrap();
            let remaining = storage.load().unwrap();
            let journal_phases: Vec<_> =
                crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                    .entries
                    .into_iter()
                    .filter_map(|(_, entry)| entry.ok().map(|entry| entry.phase))
                    .collect();
            if published_record {
                assert!(
                    remaining.is_empty(),
                    "stop errors={:?}; journal phases={journal_phases:?}",
                    result.errors
                );
                assert!(!workspace.exists());
                assert!(load_purge_fence_unlocked(session_id).unwrap().is_none());
                assert!(journal_phases.is_empty());
                assert_eq!(
                    conn.query_row(
                        "SELECT count(*) FROM acp_events WHERE session_id = ?1",
                        [session_id],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                    0
                );
            } else {
                assert_eq!(remaining.len(), 1, "stop errors={:?}", result.errors);
                assert_eq!(remaining[0].status, crate::session::Status::Deleting);
                assert!(workspace.join("writer-data").exists());
                let fence = load_purge_fence_unlocked(session_id).unwrap().unwrap();
                assert_eq!(fence.runner_pid, Some(pid));
                assert_eq!(fence.runner_generation, None);
                assert!(socket_exists(&control));
                assert_eq!(
                    journal_phases,
                    vec![crate::session::lifecycle_journal::LifecyclePhase::TeardownStarted]
                );
                assert_eq!(
                    conn.query_row(
                        "SELECT count(*) FROM acp_events WHERE session_id = ?1",
                        [session_id],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                    1
                );
            }
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM acp_events WHERE session_id = 'witness'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
        }
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
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn purge_retains_resources_when_a_socket_is_replaced_after_verified_term() {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        struct ClearSignalHook;
        impl Drop for ClearSignalHook {
            fn drop(&mut self) {
                AFTER_FENCED_RUNNER_SIGNAL.with(|hook| *hook.borrow_mut() = None);
            }
        }
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let session_id = "late-stop";
        let child_marker = workers_dir().unwrap().join("child.pid");
        let (mut child, pid, control, termination_marker) =
            spawn_control_socket_runner(session_id, false, None, Some(&child_marker));
        let _kill_group = KillProcessGroupOnDrop(pid);
        let old =
            new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5);
        save(&old).unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(session_id).unwrap();
        std::fs::write(workspace.join("retained"), b"late unknown artifact").unwrap();
        let storage = crate::session::Storage::new_unwatched("profile").unwrap();
        let mut instance = crate::session::Instance::new("late-stop", workspace.to_str().unwrap());
        instance.id = session_id.into();
        instance.source_profile = "profile".into();
        instance.view = crate::session::View::Structured;
        instance.scratch = true;
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                session_id: session_id.into(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        let committed = transaction.run_hooks().begin_irreversible().unwrap();
        let _clear_hook = ClearSignalHook;
        let replacement_path = control.clone();
        AFTER_FENCED_RUNNER_SIGNAL.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while !(termination_marker.exists() && child_marker.exists()) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "graceful shutdown barrier timed out"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert!(runner_group_alive(pid, None));
                std::fs::remove_file(&replacement_path).unwrap();
                let replacement =
                    std::os::unix::net::UnixListener::bind(&replacement_path).unwrap();
                drop(replacement);
                crate::process::worker::kill_process_group(pid);
                while runner_group_alive(pid, None) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owned group exit timed out"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }))
        });
        let result = committed.finish();
        child.wait().unwrap();
        assert!(!runner_group_alive(pid, None));
        assert!(!result.success);
        assert!(workspace.join("retained").exists());
        assert!(socket_exists(&control));
        assert_eq!(crate::process::worker::peer_pid_from_socket(&control), None);
        assert!(same_runner(&load(session_id).unwrap().unwrap(), &old));
        assert_eq!(
            storage.load().unwrap()[0].status,
            crate::session::Status::Deleting
        );
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    #[serial]
    fn zombie_only_runner_group_does_not_block_purge() {
        use std::os::unix::process::CommandExt as _;
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let child = KillOnDrop(
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let output = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned child did not become zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let session_id = "zombie";
        save(&new_record(session_id, pid, socket_path_for(session_id).unwrap()).with_generation(5))
            .unwrap();
        fence_for_purge(session_id, "profile", 9).unwrap();
        let result = terminate_and_confirm_stopped(session_id, "profile", 9);
        drop(child);
        assert!(
            result.is_ok(),
            "non-writing zombie prevented purge completion: {result:?}"
        );
        assert!(load(session_id).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn queued_publication_cannot_recreate_a_renamed_profile_reference() {
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::Storage::new_unwatched("queued-before").unwrap();
        let session_id = "queued";
        let mut instance = crate::session::Instance::new("queued", "/repo");
        instance.id = session_id.into();
        instance.view = crate::session::View::Structured;
        storage
            .update(|rows, _| {
                rows.push(instance);
                Ok(())
            })
            .unwrap();
        let mut record = new_record(
            session_id,
            std::process::id(),
            socket_path_for(session_id).unwrap(),
        );
        record.source_profile = Some("queued-before".into());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (publish_tx, publish_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            publish_rx.recv().unwrap();
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { publish_runner(&record).map(drop) })
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        let renamed = crate::session::rename_profile("queued-before", "queued-after");
        publish_tx.send(()).unwrap();
        let published = publisher.join().unwrap();
        renamed.unwrap();
        assert!(
            published.is_err(),
            "queued runner published stale profile after rename"
        );
        assert!(!crate::session::get_profile_dir_path("queued-before")
            .unwrap()
            .exists());
        assert!(load(session_id).unwrap().is_none());
        assert!(!socket_exists(
            &crate::process::worker::control_socket_sibling(&socket_path_for(session_id).unwrap())
        ));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn unknown_or_reused_kernel_birth_never_signals_a_live_canonical_peer() {
        for legacy in [false, true] {
            let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let id = "birth";
            let (mut child, pid, control, term) =
                spawn_control_socket_runner(id, false, None, None);
            let _group = KillProcessGroupOnDrop(pid);
            let mut record = new_record(id, pid, socket_path_for(id).unwrap()).with_generation(5);
            if legacy {
                record.runner_identity = None;
            } else {
                record
                    .runner_identity
                    .as_mut()
                    .unwrap()
                    .start_id
                    .push_str("-reused");
            }
            save(&record).unwrap();
            fence_for_purge(id, "profile", 7).unwrap();
            assert!(terminate_and_confirm_stopped(id, "profile", 7).is_err());
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            while std::time::Instant::now() < deadline {
                assert!(!term.exists());
                assert!(is_pid_alive(pid));
                assert_eq!(
                    crate::process::worker::peer_pid_from_socket(&control),
                    Some(pid)
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(same_runner(&load(id).unwrap().unwrap(), &record));
            assert!(is_purge_fenced(id).unwrap());
            crate::process::worker::kill_process_group(pid);
            child.wait().unwrap();
            terminate_and_confirm_stopped(id, "profile", 7)
                .expect("retry must complete after external settlement");
            assert!(load(id).unwrap().is_none());
            assert!(is_purge_fenced(id).unwrap());
            assert!(clear_purge_fence_if_owned(id, "profile", 7).unwrap());
            assert!(!is_purge_fenced(id).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn explicit_recovery_retires_unknown_birth_without_touching_a_live_pid() {
        use std::os::unix::process::CommandExt as _;

        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let id = "unknown-birth";
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let _group = KillProcessGroupOnDrop(pid);
        let mut record = new_record(id, pid, socket_path_for(id).unwrap()).with_generation(5);
        record.runner_identity = None;
        save(&record).unwrap();
        fence_for_purge(id, "profile", 7).unwrap();

        recover_worker(id).unwrap();

        assert!(is_pid_alive(pid));
        assert!(load(id).unwrap().is_none());
        let fence = load_purge_fence_unlocked(id).unwrap().unwrap();
        assert!(fence.runner_pid.is_none());
        assert!(fence.runner_identity.is_none());
        terminate_and_confirm_stopped(id, "profile", 7).unwrap();
        assert!(clear_purge_fence_if_owned(id, "profile", 7).unwrap());
        assert!(is_pid_alive(pid));
        crate::process::worker::kill_process_group(pid);
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn zombie_leader_with_a_live_group_member_still_blocks_purge() {
        use std::os::unix::process::CommandExt as _;
        let temp = TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let release = temp.path().join("release");
        let mut leader = KillOnDrop(
            std::process::Command::new("sh")
                .args([
                    "-c",
                    r#"while ! test -f "$1"; do sleep 0.01; done; exit 0"#,
                    "sh",
                ])
                .arg(&release)
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pid = leader.0.id();
        let _group = KillProcessGroupOnDrop(pid);
        let mut member = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(pid as i32)
                .spawn()
                .unwrap(),
        );
        let record = new_record("zombie-live", pid, socket_path_for("zombie-live").unwrap())
            .with_generation(5);
        save(&record).unwrap();
        fence_for_purge("zombie-live", "profile", 7).unwrap();
        std::fs::write(&release, b"exit").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&state.stdout)
                .trim()
                .starts_with('Z')
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(is_pid_alive(member.0.id()));
        let result = terminate_and_confirm_stopped("zombie-live", "profile", 7);
        assert!(is_pid_alive(member.0.id()));
        crate::process::worker::kill_process_group(pid);
        member.0.wait().unwrap();
        leader.0.wait().unwrap();
        assert!(result.is_err());
        assert!(same_runner(&load("zombie-live").unwrap().unwrap(), &record));
    }
    #[cfg(unix)]
    #[test]
    #[serial]
    fn fenced_native_evidence_survives_runner_record_disappearance() {
        let temp = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let id = "fenced-native-snapshot";
        let runtime =
            crate::process::prime::PrimeRuntime::new("/bin/false".into(), temp.path().into(), None)
                .unwrap();
        runtime.prepare().unwrap();
        runtime.adopt_prepared().unwrap();
        std::fs::write(runtime.directory.join("frontend.identity"), "invalid").unwrap();
        let mut record = new_record(id, std::process::id(), socket_path_for(id).unwrap());
        record.agent_key = "prime-agent".into();
        record.prime_runtime = Some(runtime.clone());
        record.runner_identity.as_mut().unwrap().boot_id = uuid::Uuid::new_v4().to_string();
        save(&record).unwrap();
        fence_for_purge(id, "profile", 7).unwrap();
        std::fs::remove_file(record_path(id).unwrap()).unwrap();
        assert!(terminate_and_confirm_stopped(id, "profile", 7).is_err());
        let fence = load_purge_fence_unlocked(id).unwrap().unwrap();
        let retained: WorkerRecord = serde_json::from_value(fence.runner_record.unwrap()).unwrap();
        assert!(same_runner(&record, &retained));
        assert_eq!(retained.prime_runtime.unwrap().directory, runtime.directory);
        assert!(runtime.directory.exists());
        std::fs::remove_dir_all(&runtime.directory).unwrap();
    }
}

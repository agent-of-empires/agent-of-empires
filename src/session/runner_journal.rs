//! Execution coverage survives registry cleanup and daemon replacement.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use super::deletion::SessionPathOwner;
use super::storage::{same_filesystem_identity, sync_parent_directory};
use super::{Instance, LifecycleOperation, Storage};

pub(crate) type BootToken = [u8; 16];

static BOOT: LazyLock<Option<BootToken>> = LazyLock::new(|| {
    let boot = Uuid::parse_str(crate::process::boot_id()?.trim()).ok()?;
    (!boot.is_nil()).then_some(*boot.as_bytes())
});

pub(crate) fn current_boot() -> Option<BootToken> {
    *BOOT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunnerLaunch {
    nonce: [u8; 16],
    boot: BootToken,
    generation: u64,
    incarnation: Option<crate::process::ProcessIncarnation>,
}

impl RunnerLaunch {
    fn is_quiescent(&self, boot: BootToken) -> bool {
        let Some(incarnation) = self.incarnation else {
            // Authorization is written only after publishing the incarnation.
            return true;
        };
        if self.boot != boot {
            return true;
        }
        if !(2..=i32::MAX as u32).contains(&incarnation.pid) || incarnation.group != incarnation.pid
        {
            return false;
        }
        if crate::process::process_namespace().ok() != Some(incarnation.namespace) {
            return false;
        }
        if !crate::process::worker::is_process_group_alive(incarnation.group) {
            return true;
        }
        // A populated group reserves its numeric id, even after its leader exits.
        matches!(
            crate::process::process_incarnation(incarnation.pid),
            Ok(Some(current)) if current.start != incarnation.start
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "coverage", rename_all = "snake_case")]
enum Coverage {
    Complete,
    Unknown { boot: Option<BootToken> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunnerExecutionJournal {
    #[serde(flatten)]
    coverage: Coverage,
    launches: Vec<RunnerLaunch>,
}

impl Default for RunnerExecutionJournal {
    fn default() -> Self {
        Self {
            coverage: Coverage::Unknown {
                boot: current_boot(),
            },
            launches: Vec::new(),
        }
    }
}

impl RunnerExecutionJournal {
    pub(crate) fn new() -> Self {
        Self {
            coverage: Coverage::Complete,
            launches: Vec::new(),
        }
    }

    fn launches(&self) -> &[RunnerLaunch] {
        &self.launches
    }

    fn launches_mut(&mut self) -> &mut Vec<RunnerLaunch> {
        &mut self.launches
    }

    fn refresh(&mut self, boot: BootToken, retain_nonce: Option<[u8; 16]>) -> bool {
        let mut changed = false;
        if let Coverage::Unknown { boot: previous } = &mut self.coverage {
            match previous {
                Some(previous) if *previous != boot => {
                    self.coverage = Coverage::Complete;
                    changed = true;
                }
                None => {
                    *previous = Some(boot);
                    changed = true;
                }
                _ => {}
            }
        }
        let count = self.launches.len();
        self.launches
            .retain(|launch| retain_nonce == Some(launch.nonce) || !launch.is_quiescent(boot));
        changed || self.launches.len() != count
    }

    pub(crate) fn proves_quiescent(&self) -> bool {
        self.proves_for(None)
    }

    fn proves_for(&self, nonce: Option<[u8; 16]>) -> bool {
        let Some(boot) = current_boot() else {
            return false;
        };
        let covered = match self.coverage {
            Coverage::Complete => true,
            Coverage::Unknown { boot: previous } => {
                previous.is_some_and(|previous| previous != boot)
            }
        };
        let mut found = false;
        let stopped = self
            .launches
            .iter()
            .filter(|launch| nonce.is_none_or(|nonce| launch.nonce == nonce))
            .all(|launch| {
                found = true;
                launch.is_quiescent(boot)
            });
        stopped && (covered || nonce.is_some() && found)
    }
}

fn ensure_unique_owner(storage: &Storage, id: &str) -> Result<()> {
    let directory = storage
        .sessions_path()
        .parent()
        .context("sessions path has no parent")?;
    let owner = std::fs::metadata(directory)?;
    for profile in super::list_profiles_for_worktree_inventory()? {
        let other = Storage::open_unwatched(&profile)?;
        let directory = other
            .sessions_path()
            .parent()
            .context("sessions path has no parent")?;
        if same_filesystem_identity(&owner, &std::fs::metadata(directory)?) {
            continue;
        }
        anyhow::ensure!(
            !other
                .load_strict_for_worktree_ownership_locked()?
                .iter()
                .any(|row| row.id == id),
            "session id {id} has owners in distinct profiles; runner routing is ambiguous"
        );
    }
    Ok(())
}

fn startable(row: &Instance) -> Result<()> {
    row.ensure_startable()?;
    if row.has_fresh_lifecycle_reservation(chrono::Utc::now()) {
        let operation = row.lifecycle_reservation.as_ref().unwrap().op;
        anyhow::ensure!(
            matches!(
                operation,
                LifecycleOperation::Launch | LifecycleOperation::Capture
            ),
            "session is reserved for {operation:?}"
        );
    }
    Ok(())
}

pub(crate) struct ManagedLaunch<'a> {
    owner: SessionPathOwner<'a>,
    nonce: Uuid,
    generation: u64,
}

impl<'a> ManagedLaunch<'a> {
    pub(crate) fn new(owner: SessionPathOwner<'a>, generation: u64) -> Result<Self> {
        anyhow::ensure!(
            !owner.profile.is_empty() && !owner.session_id.is_empty(),
            "detached runner requires an explicit stored owner"
        );
        Ok(Self {
            owner,
            nonce: Uuid::new_v4(),
            generation,
        })
    }

    pub(crate) fn nonce(&self) -> Uuid {
        self.nonce
    }

    pub(crate) fn configure(&self, command: &mut tokio::process::Command) {
        command.arg("--managed-profile").arg(self.owner.profile);
        command.arg("--launch-nonce").arg(self.nonce.to_string());
    }

    pub(crate) fn spawn(
        self,
        command: &mut tokio::process::Command,
        mut capture: impl FnMut(crate::acp::runner_lifecycle::RunnerIdentity),
    ) -> Result<u32> {
        let boot = current_boot().context("verified boot identity is unavailable")?;
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        let storage = Storage::open_unwatched(self.owner.profile)?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock(self.owner.session_id)?;
        ensure_unique_owner(&storage, self.owner.session_id)?;
        let nonce = *self.nonce.as_bytes();
        let mut launch_reservation = None;
        let launched = (|| -> Result<u32> {
            storage.update_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.owner.session_id)
                    .context("managed runner's session no longer exists")?;
                startable(row)?;
                let now = chrono::Utc::now();
                if !row.has_fresh_lifecycle_reservation(now) {
                    launch_reservation = Some(row.try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        now,
                    )?);
                }
                row.runner_journal.refresh(boot, None);
                anyhow::ensure!(
                    matches!(row.runner_journal.coverage, Coverage::Complete)
                        && row.runner_journal.launches().is_empty(),
                    "runner history does not prove quiescence; fresh launch remains protected"
                );
                row.runner_journal.launches_mut().push(RunnerLaunch {
                    nonce,
                    boot,
                    generation: self.generation,
                    incarnation: None,
                });
                Ok(())
            })?;
            sync_parent_directory(storage.sessions_path())?;
            let (mut authorization, input) = std::os::unix::net::UnixStream::pair()?;
            let input: std::os::fd::OwnedFd = input.into();
            command.stdin(std::process::Stdio::from(input));
            let mut child = command.spawn()?;
            let pid = child.id().context("runner exited before identification")?;
            let published = (|| -> Result<()> {
                let incarnation = crate::process::process_incarnation(pid)?
                    .context("runner incarnation is unavailable")?;
                anyhow::ensure!(
                    incarnation.group == pid,
                    "runner does not lead its process group"
                );
                storage.update_under_workspace_claim_lock(|rows, _| {
                    let row = rows
                        .iter_mut()
                        .find(|row| row.id == self.owner.session_id)
                        .context("managed runner's session disappeared")?;
                    startable(row)?;
                    let launch = row
                        .runner_journal
                        .launches_mut()
                        .iter_mut()
                        .find(|launch| launch.nonce == nonce && launch.incarnation.is_none())
                        .context("runner authorization was superseded")?;
                    launch.incarnation = Some(incarnation);
                    Ok(())
                })?;
                sync_parent_directory(storage.sessions_path())?;
                capture(crate::acp::runner_lifecycle::RunnerIdentity {
                    pid,
                    generation: self.generation,
                    launch_nonce: Some(self.nonce),
                });
                authorization.write_all(&nonce)?;
                Ok(())
            })();
            drop(authorization);
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            published?;
            Ok(pid)
        })();
        let released = if let Some(generation) = launch_reservation {
            storage.update_under_workspace_claim_lock(|rows, _| {
                if let Some(row) = rows.iter_mut().find(|row| row.id == self.owner.session_id) {
                    row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation,
                    );
                }
                Ok(())
            })
        } else {
            Ok(())
        };
        let pid = launched?;
        released?;
        Ok(pid)
    }
}

pub(crate) fn accept_authorization(
    profile: &str,
    id: &str,
    nonce: Uuid,
    generation: u64,
) -> Result<()> {
    let mut received = [0; 16];
    std::io::stdin()
        .read_exact(&mut received)
        .context("runner authorization closed")?;
    anyhow::ensure!(
        received == *nonce.as_bytes(),
        "runner authorization nonce differs"
    );
    anyhow::ensure!(!profile.is_empty(), "runner has no stored owner");
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let pid = std::process::id();
    let incarnation =
        crate::process::process_incarnation(pid)?.context("runner incarnation is unavailable")?;
    anyhow::ensure!(incarnation.group == pid, "runner is not its group leader");
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let storage = Storage::open_unwatched(profile)?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(&storage, id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("runner's session disappeared")?;
    startable(&row)?;
    anyhow::ensure!(
        row.runner_journal.launches().iter().any(|launch| {
            launch.nonce == received
                && launch.boot == boot
                && launch.generation == generation
                && launch.incarnation == Some(incarnation)
        }),
        "runner lacks published execution authorization"
    );
    Ok(())
}

pub(crate) fn stop_socket(id: &str, pid: u32) -> Result<PathBuf> {
    let record = crate::process::worker_registry::record_path(id)?;
    Ok(record.with_file_name(format!("{id}.{pid}.stop")))
}

pub(crate) async fn wait_for_stop(listener: tokio::net::UnixListener, nonce: Uuid) -> Result<bool> {
    loop {
        let (mut connection, _) = listener.accept().await?;
        let mut received = [0; 17];
        if matches!(
            tokio::time::timeout(Duration::from_secs(2), connection.read_exact(&mut received))
                .await,
            Ok(Ok(_))
        ) && received[..16] == nonce.as_bytes()[..]
            && received[16] <= 1
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                connection.write_all(nonce.as_bytes()),
            )
            .await;
            return Ok(received[16] == 1);
        }
    }
}

pub(crate) fn unique_stored_owner(id: &str) -> Result<String> {
    find_stored_owner(id)?.context("runner has no authoritative stored owner")
}

fn find_stored_owner(id: &str) -> Result<Option<String>> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    find_stored_owner_locked(id)
}

fn find_stored_owner_locked(id: &str) -> Result<Option<String>> {
    let mut found: Option<(String, std::fs::Metadata)> = None;
    for profile in super::list_profiles_for_worktree_inventory()? {
        let storage = Storage::open_unwatched(&profile)?;
        if !storage
            .load_strict_for_worktree_ownership_locked()?
            .iter()
            .any(|row| row.id == id)
        {
            continue;
        }
        let metadata = std::fs::metadata(
            storage
                .sessions_path()
                .parent()
                .context("sessions path has no parent")?,
        )?;
        match &found {
            Some((_, previous)) => anyhow::ensure!(
                same_filesystem_identity(previous, &metadata),
                "session id {id} has owners in distinct profiles; runner routing is ambiguous"
            ),
            None => found = Some((profile, metadata)),
        }
    }
    Ok(found.map(|(profile, _)| profile))
}

pub(crate) async fn settle_captured_ticket(
    id: &str,
    identity: crate::acp::runner_lifecycle::RunnerIdentity,
    force: bool,
) -> Result<()> {
    let read_id = id.to_owned();
    let profile = tokio::task::spawn_blocking(move || find_stored_owner(&read_id)).await??;
    if let Some(profile) = profile {
        let owner = SessionPathOwner {
            profile: &profile,
            session_id: id,
        };
        match identity.launch_nonce {
            Some(nonce) => settle_selected(owner, Some(*nonce.as_bytes()), force, None).await?,
            None => require_quiescent(owner, None).await?,
        }
    } else {
        let nonce = identity
            .launch_nonce
            .context("orphan lacks an authenticated execution ticket")?;
        let read_id = id.to_owned();
        let _fences = tokio::task::spawn_blocking(move || {
            let workspace = super::acquire_session_workspace_claim_lock()?;
            let identities = super::acquire_session_identity_lock()?;
            anyhow::ensure!(
                find_stored_owner_locked(&read_id)?.is_none(),
                "orphan session acquired a stored owner"
            );
            let namespace = crate::process::process_namespace()?;
            anyhow::ensure!(
                (2..=i32::MAX as u32).contains(&identity.pid),
                "invalid orphan process id"
            );
            let incarnation = crate::process::process_incarnation(identity.pid)?
                .context("orphan lacks a live kernel birth observation")?;
            anyhow::ensure!(
                incarnation.group == identity.pid && incarnation.namespace == namespace,
                "orphan lacks a local leader identity"
            );
            anyhow::Ok((workspace, identities))
        })
        .await??;
        let path = stop_socket(id, identity.pid)?;
        let (path, peer) = tokio::task::spawn_blocking(move || {
            let peer = crate::process::worker::peer_pid_from_socket(&path);
            (path, peer)
        })
        .await?;
        anyhow::ensure!(
            peer == Some(identity.pid),
            "orphan stop endpoint has no matching kernel peer"
        );
        let mut frame = [0; 17];
        frame[..16].copy_from_slice(nonce.as_bytes());
        frame[16] = u8::from(force);
        let receipt = tokio::time::timeout(Duration::from_secs(1), async {
            let mut socket = tokio::net::UnixStream::connect(path).await?;
            socket.write_all(&frame).await?;
            let mut receipt = [0; 16];
            socket.read_exact(&mut receipt).await?;
            Ok::<_, std::io::Error>(receipt)
        })
        .await
        .context("orphan stop endpoint timed out")??;
        anyhow::ensure!(
            receipt == *nonce.as_bytes(),
            "orphan execution ticket was not authenticated"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while crate::process::worker::is_process_group_alive(identity.pid)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::ensure!(
            !crate::process::worker::is_process_group_alive(identity.pid),
            "orphan group is not proven quiescent"
        );
    }
    anyhow::ensure!(
        crate::process::worker_registry::delete_if_owned_by(
            id,
            identity.pid,
            identity.generation,
            identity.launch_nonce,
        ),
        "settled runner registry record could not be retired"
    );
    Ok(())
}

fn snapshot(
    owner: SessionPathOwner<'_>,
    nonce: Option<[u8; 16]>,
    reservation: Option<(LifecycleOperation, u64)>,
) -> Result<RunnerExecutionJournal> {
    anyhow::ensure!(
        !owner.profile.is_empty(),
        "session has no explicit stored owner"
    );
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let storage = Storage::open_unwatched(owner.profile)?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(owner.session_id)?;
    ensure_unique_owner(&storage, owner.session_id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == owner.session_id)
        .context("runner's stored owner disappeared")?;
    if let Some((operation, generation)) = reservation {
        anyhow::ensure!(
            row.lifecycle_reservation_is_owned(operation, generation),
            "lifecycle reservation was superseded before runner settlement"
        );
    }
    let mut journal = row.runner_journal;
    if journal.refresh(boot, nonce) {
        storage.update_under_workspace_claim_lock(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == owner.session_id)
                .context("runner's stored owner disappeared")?;
            row.runner_journal = journal.clone();
            Ok(())
        })?;
        sync_parent_directory(storage.sessions_path())?;
    }
    Ok(journal)
}

pub(crate) async fn settle_unique(id: &str) -> Result<()> {
    let owned = id.to_owned();
    let profile = tokio::task::spawn_blocking(move || unique_stored_owner(&owned)).await??;
    settle(
        SessionPathOwner {
            profile: &profile,
            session_id: id,
        },
        None,
    )
    .await
}

pub(crate) async fn kill_unique(id: &str) -> Result<()> {
    let owned = id.to_owned();
    let profile = tokio::task::spawn_blocking(move || unique_stored_owner(&owned)).await??;
    settle_selected(
        SessionPathOwner {
            profile: &profile,
            session_id: id,
        },
        None,
        true,
        None,
    )
    .await
}

pub(crate) async fn settle(
    owner: SessionPathOwner<'_>,
    reservation: Option<(LifecycleOperation, u64)>,
) -> Result<()> {
    settle_selected(owner, None, false, reservation).await
}

pub(crate) async fn require_quiescent(
    owner: SessionPathOwner<'_>,
    reservation: Option<(LifecycleOperation, u64)>,
) -> Result<()> {
    let profile = owner.profile.to_owned();
    let id = owner.session_id.to_owned();
    let journal = tokio::task::spawn_blocking(move || {
        snapshot(
            SessionPathOwner {
                profile: &profile,
                session_id: &id,
            },
            None,
            reservation,
        )
    })
    .await??;
    anyhow::ensure!(
        journal.proves_quiescent(),
        "runner execution is not proven quiescent; no runner was signalled"
    );
    Ok(())
}

fn for_each_stored_session(mut visit: impl FnMut(Instance)) -> Result<()> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let mut directories = Vec::new();
    for profile in super::list_profiles_for_worktree_inventory()? {
        let storage = Storage::open_unwatched(&profile)?;
        let metadata = std::fs::metadata(
            storage
                .sessions_path()
                .parent()
                .context("sessions path has no parent")?,
        )?;
        if directories
            .iter()
            .any(|previous| same_filesystem_identity(previous, &metadata))
        {
            continue;
        }
        directories.push(metadata);
        for row in storage.load_strict_for_worktree_ownership_locked()? {
            visit(row);
        }
    }
    Ok(())
}

pub(crate) fn stored_session_ids() -> Result<Vec<String>> {
    let mut ids = std::collections::HashSet::new();
    for_each_stored_session(|row| {
        ids.insert(row.id);
    })?;
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_unstable();
    Ok(ids)
}

pub(crate) fn retained_runner_session_ids() -> Result<std::collections::HashSet<String>> {
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let mut ids = std::collections::HashSet::new();
    for_each_stored_session(|row| {
        if row
            .runner_journal
            .launches()
            .iter()
            .any(|launch| !launch.is_quiescent(boot))
        {
            ids.insert(row.id);
        }
    })?;
    Ok(ids)
}

pub(crate) async fn settle_nonce(
    owner: SessionPathOwner<'_>,
    nonce: Uuid,
    reservation: Option<(LifecycleOperation, u64)>,
) -> Result<()> {
    settle_selected(owner, Some(*nonce.as_bytes()), false, reservation).await
}

pub(crate) fn verify_published_runner(
    owner: SessionPathOwner<'_>,
    nonce: Uuid,
    pid: u32,
    generation: u64,
) -> Result<()> {
    anyhow::ensure!(
        !owner.profile.is_empty(),
        "runner has no explicit stored owner"
    );
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let storage = Storage::open_unwatched(owner.profile)?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(owner.session_id)?;
    ensure_unique_owner(&storage, owner.session_id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == owner.session_id)
        .context("runner's stored owner disappeared")?;
    let incarnation =
        crate::process::process_incarnation(pid)?.context("runner incarnation is absent")?;
    anyhow::ensure!(
        incarnation.group == pid && crate::process::worker::is_process_group_alive(pid),
        "runner group is not live"
    );
    anyhow::ensure!(
        row.runner_journal
            .launches
            .iter()
            .any(|launch| launch.nonce == *nonce.as_bytes()
                && launch.boot == boot
                && launch.generation == generation
                && launch.incarnation == Some(incarnation)),
        "runner's published execution does not match its current birth"
    );
    let record = crate::process::worker_registry::load_strict(owner.session_id)?
        .context("runner record disappeared")?;
    anyhow::ensure!(
        record.pid == pid && record.generation == generation && record.launch_nonce == Some(nonce),
        "runner registry execution was replaced"
    );
    let profile = record
        .source_profile
        .as_deref()
        .filter(|profile| !profile.is_empty())
        .context("runner record has no explicit stored owner")?;
    let other = Storage::open_unwatched(profile)?;
    let own = std::fs::metadata(
        storage
            .sessions_path()
            .parent()
            .context("sessions path has no parent")?,
    )?;
    let recorded = std::fs::metadata(
        other
            .sessions_path()
            .parent()
            .context("sessions path has no parent")?,
    )?;
    anyhow::ensure!(
        same_filesystem_identity(&own, &recorded),
        "runner record belongs to another physical profile"
    );
    Ok(())
}

async fn settle_selected(
    owner: SessionPathOwner<'_>,
    nonce: Option<[u8; 16]>,
    force: bool,
    reservation: Option<(LifecycleOperation, u64)>,
) -> Result<()> {
    let profile = owner.profile.to_owned();
    let id = owner.session_id.to_owned();
    let profile_read = profile.clone();
    let id_read = id.clone();
    let journal = tokio::task::spawn_blocking(move || {
        snapshot(
            SessionPathOwner {
                profile: &profile_read,
                session_id: &id_read,
            },
            nonce,
            reservation,
        )
    })
    .await??;
    if !journal.proves_for(nonce) {
        let boot = current_boot().context("verified boot identity is unavailable")?;
        for launch in journal.launches().iter().filter(|launch| {
            nonce.is_none_or(|nonce| launch.nonce == nonce) && !launch.is_quiescent(boot)
        }) {
            let Some(incarnation) = launch.incarnation else {
                continue;
            };
            let path = stop_socket(&id, incarnation.pid)?;
            let mut frame = [0; 17];
            frame[..16].copy_from_slice(&launch.nonce);
            frame[16] = u8::from(force);
            let requested = tokio::time::timeout(Duration::from_secs(1), async {
                let mut socket = tokio::net::UnixStream::connect(path).await?;
                socket.write_all(&frame).await
            })
            .await;
            if !matches!(requested, Ok(Ok(()))) {
                tracing::debug!(session = %id, pid = incarnation.pid, "runner stop endpoint unavailable");
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while journal.launches().iter().any(|launch| {
            nonce.is_none_or(|nonce| launch.nonce == nonce) && !launch.is_quiescent(boot)
        }) && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let refreshed = tokio::task::spawn_blocking(move || {
        snapshot(
            SessionPathOwner {
                profile: &profile,
                session_id: &id,
            },
            nonce,
            reservation,
        )
    })
    .await??;
    anyhow::ensure!(refreshed.proves_for(nonce), "runner execution is not proven quiescent; retain the session and checkout. Legacy unknown history requires a verified boot change");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stale_trash_settlement_never_stops_replacement() {
        let temporary = tempfile::TempDir::new_in("/tmp").unwrap();
        let _app_dir = super::super::test_support::isolate_app_dir_at(temporary.path());
        super::super::create_profile("proof").unwrap();
        let storage = Storage::open_unwatched("proof").unwrap();
        let mut row = Instance::new("replacement", temporary.path().to_str().unwrap());
        let old_generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        let id = row.id.clone();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let child = ChildGuard(
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let incarnation = crate::process::process_incarnation(child.0.id())
            .unwrap()
            .unwrap();
        let nonce = Uuid::new_v4();
        let boot = current_boot().unwrap();
        storage
            .update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                assert!(row.release_lifecycle_reservation_if_owned(
                    LifecycleOperation::Trash,
                    old_generation
                ));
                let generation = row
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .unwrap();
                row.runner_journal = RunnerExecutionJournal {
                    coverage: Coverage::Complete,
                    launches: vec![RunnerLaunch {
                        nonce: *nonce.as_bytes(),
                        boot,
                        generation,
                        incarnation: Some(incarnation),
                    }],
                };
                row.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
                Ok(())
            })
            .unwrap();
        let endpoint =
            std::os::unix::net::UnixListener::bind(stop_socket(&id, child.0.id()).unwrap())
                .unwrap();
        endpoint.set_nonblocking(true).unwrap();
        let outcome = settle(
            SessionPathOwner {
                profile: "proof",
                session_id: &id,
            },
            Some((LifecycleOperation::Trash, old_generation)),
        )
        .await;
        assert!(outcome.is_err());
        assert!(
            matches!(endpoint.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "stale Trash sent a stop request to the replacement execution"
        );
        let retained = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert!(retained.lifecycle_generation > old_generation);
        assert_eq!(retained.runner_journal.launches[0].nonce, *nonce.as_bytes());
        assert!(crate::process::worker::is_process_group_alive(child.0.id()));
    }
}

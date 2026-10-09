//! Execution coverage survives registry cleanup and daemon replacement.

use crate::process::worker_registry::SocketEndpointIdentity;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

fn mark_original_refusal<C: custody::OriginalChild>(
    custody: &custody::OriginalLaunchCustody<C>,
    nonce: [u8; 16],
    boot: BootToken,
) -> Result<RunnerLaunch> {
    let (original, attempted, identity) = {
        let state = custody.lock();
        (
            state.original.clone(),
            state.authorization_attempted,
            state.identity,
        )
    };
    let mut slots = original.births.iter().filter(|launch| {
        launch.nonce == nonce && launch.boot == boot && launch.generation == original.generation()
    });
    let mut key = slots
        .next()
        .context("original refusal has no acquired Natal slot")?
        .birth_key();
    anyhow::ensure!(
        slots.next().is_none(),
        "original refusal has duplicate Natal slots"
    );
    if key.incarnation.is_none() {
        if let Some(identity) = identity.filter(|identity| identity.incarnation.is_some()) {
            anyhow::ensure!(
                identity.launch_nonce == Some(uuid::Uuid::from_bytes(key.nonce))
                    && identity.boot == Some(key.boot)
                    && identity.generation == key.generation
                    && identity.profile_identity == key.profile_identity,
                "original refusal belongs to another observed native birth"
            );
            key.incarnation = identity.incarnation;
        }
    }
    let launch = record_unresolved_launch_under_original_fences(&original, key, attempted, None)?;
    let unresolved = original.with_launch_snapshot(launch.clone())?;
    custody
        .admission
        .record_produced_origin(&original, unresolved.clone(), identity)?;
    custody.lock().original = unresolved;
    Ok(launch)
}

fn retire_original_no_target(
    receipt: &OriginalNoTargetReceipt,
) -> Result<std::sync::Arc<OriginalNoTargetRemovalAck>> {
    let original = receipt.original();
    let storage = original.storage();
    let key = original_no_target_patch_key(receipt)?;
    storage.verify_profile_identity()?;
    ensure_unique_owner(storage, original.session_id())?;
    let written = storage.update_no_target_under_workspace_claim_lock(receipt, |rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == original.session_id())
            .context("original no-target owner disappeared")?;
        original.validate_row(row)?;
        let count = row
            .runner_journal
            .launches()
            .iter()
            .filter(|launch| *launch == receipt.launch())
            .count();
        anyhow::ensure!(
            count == 1,
            "original no-target removal lost its exact acknowledged launch snapshot"
        );
        let record = crate::process::worker_registry::load_strict(original.session_id())?;
        match (&receipt.launch().registry, record) {
            (Some(witness), Some(record)) => {
                anyhow::ensure!(
                    receipt.launch().matches_birth(&record) && witness.matches_record(&record),
                    "original no-target registry witness was replaced"
                );
                anyhow::ensure!(
                    crate::process::worker_registry::delete_if_owned_by(&record),
                    "original no-target registry writer did not acknowledge retirement"
                );
            }
            (Some(witness), None) => {
                crate::process::worker_registry::retire_endpoint(
                    original.session_id(),
                    &crate::process::worker::control_socket_sibling(&witness.socket_path),
                    &witness.control_file_identity,
                )?;
            }
            (None, None) => {}
            (None, Some(_)) => {
                anyhow::bail!("a different registry still owns the original namespace")
            }
        }
        if let Some(endpoint) = receipt.launch().stop_endpoint {
            let identity = receipt
                .identity()
                .context("original stop endpoint has no held root birth")?;
            let path = stop_socket(original.session_id(), identity.pid)?;
            crate::process::worker_registry::retire_endpoint(
                original.session_id(),
                &path,
                &endpoint,
            )?;
        }
        row.runner_journal
            .launches_mut()
            .retain(|launch| launch.birth_key() != key);
        Ok(row.clone())
    })?;
    sync_parent_directory(storage.sessions_path())?;
    let acknowledged = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == original.session_id())
        .context("original no-target writer lost its acknowledged owner")?;
    let derived = original.derive_no_target_successor(receipt.launch(), receipt.identity())?;
    derived.validate_row(&written)?;
    derived.validate_row(&acknowledged)?;
    anyhow::ensure!(
        written.runner_journal.launches() == acknowledged.runner_journal.launches()
            && !acknowledged
                .runner_journal
                .launches()
                .iter()
                .any(|launch| launch.birth_key() == key),
        "original no-target writer did not acknowledge exact tuple removal"
    );
    Ok(std::sync::Arc::new(OriginalNoTargetRemovalAck {
        original: original.clone(),
        launch: receipt.launch().clone(),
        identity: receipt.identity(),
        derived,
    }))
}

pub(super) fn original_no_target_patch_key(
    receipt: &OriginalNoTargetReceipt,
) -> Result<NativeBirthKey> {
    if receipt.never_spawned() {
        anyhow::ensure!(
            receipt.identity().is_none()
                && receipt.launch().incarnation.is_none()
                && receipt.launch().stop_endpoint.is_none()
                && receipt.launch().registry.is_none(),
            "sealed NeverSpawned receipt replaced its original pending slot"
        );
    }
    receipt
        .original()
        .no_target_native_key(receipt.launch(), receipt.identity())
}

use super::deletion::SessionPathOwner;
use super::storage::{same_filesystem_identity, sync_parent_directory};
use super::{Instance, LifecycleOperation, Storage};

mod bootstrap;
mod custody;
#[cfg(test)]
pub(crate) use custody::ManagedChild;
pub(crate) use custody::OriginalNoTargetReceipt;
pub(crate) mod native_create;
#[cfg(debug_assertions)]
pub(crate) use bootstrap::hold_for_hosted_proof;
pub(crate) use bootstrap::LaunchBootstrap;
pub use bootstrap::RunnerNatalGuard;
pub use native_create::OwnedCreateCommand;

pub(crate) type BootToken = [u8; 16];

static BOOT: LazyLock<Option<BootToken>> = LazyLock::new(|| {
    let boot = Uuid::parse_str(crate::process::boot_id()?.trim()).ok()?;
    (!boot.is_nil()).then_some(*boot.as_bytes())
});

pub(crate) fn current_boot() -> Option<BootToken> {
    *BOOT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum NativeLaunchPhase {
    Natal,
    Armed,
    Published,
    Unresolved { may_authorize: bool },
}

impl NativeLaunchPhase {
    pub(crate) fn allows_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        matches!(
            (self, next),
            (Self::Natal, Self::Armed)
                | (Self::Armed, Self::Published)
                | (
                    Self::Natal | Self::Armed,
                    Self::Unresolved {
                        may_authorize: false
                    }
                )
                | (
                    Self::Armed | Self::Published,
                    Self::Unresolved {
                        may_authorize: true
                    }
                )
                | (
                    Self::Unresolved {
                        may_authorize: false
                    },
                    Self::Unresolved {
                        may_authorize: true
                    }
                )
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunnerLaunch {
    nonce: [u8; 16],
    boot: BootToken,
    generation: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    incarnation: Option<crate::process::ProcessIncarnation>,
    profile_identity: Option<super::storage::DirectoryIdentity>,
    phase: NativeLaunchPhase,
    stop_endpoint: Option<SocketEndpointIdentity>,
    registry: Option<RegistryWitness>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NativeBirthKey {
    nonce: [u8; 16],
    boot: BootToken,
    generation: u64,
    incarnation: Option<crate::process::ProcessIncarnation>,
    profile_identity: Option<super::DirectoryIdentity>,
}

impl RunnerLaunch {
    fn birth_key(&self) -> NativeBirthKey {
        NativeBirthKey {
            nonce: self.nonce,
            boot: self.boot,
            generation: self.generation,
            incarnation: self.incarnation,
            profile_identity: self.profile_identity,
        }
    }
}

impl RunnerLaunch {
    fn preserves_original_evidence(&self, next: &Self) -> bool {
        (self.birth_key() == next.birth_key()
            || self.phase == NativeLaunchPhase::Natal
                && matches!(
                    next.phase,
                    NativeLaunchPhase::Armed
                        | NativeLaunchPhase::Unresolved {
                            may_authorize: false
                        }
                )
                && self.incarnation.is_none()
                && next.incarnation.is_some()
                && self.nonce == next.nonce
                && self.boot == next.boot
                && self.generation == next.generation
                && self.profile_identity == next.profile_identity
                && self.stop_endpoint.is_none()
                && self.registry.is_none())
            && self.phase.allows_transition_to(next.phase)
            && self
                .stop_endpoint
                .is_none_or(|endpoint| next.stop_endpoint == Some(endpoint))
            && match (&self.registry, &next.registry) {
                (None, _) => true,
                (Some(before), Some(after)) => {
                    before.control_file_identity == after.control_file_identity
                        && before.socket_path == after.socket_path
                        && before.record_file_identity.is_durable()
                        && after.record_file_identity.is_durable()
                }
                (Some(_), None) => false,
            }
    }

    fn is_published(&self) -> bool {
        self.phase == NativeLaunchPhase::Published
            && self.incarnation.is_some()
            && self
                .profile_identity
                .is_some_and(|identity| identity.is_durable())
            && self
                .stop_endpoint
                .is_some_and(|endpoint| endpoint.is_durable())
            && self.registry.as_ref().is_some_and(|witness| {
                witness.record_file_identity.is_durable()
                    && witness.control_file_identity.is_durable()
            })
    }
}

fn validate_launch_snapshots(launches: &[RunnerLaunch]) -> Result<()> {
    for (index, launch) in launches.iter().enumerate() {
        anyhow::ensure!(
            !launches[..index].iter().any(|before| {
                before.nonce == launch.nonce
                    && before.boot == launch.boot
                    && before.generation == launch.generation
            }),
            "duplicate original native launch slot"
        );
        match launch.phase {
            NativeLaunchPhase::Natal => anyhow::ensure!(
                launch.incarnation.is_none()
                    && launch.stop_endpoint.is_none()
                    && launch.registry.is_none(),
                "Natal launch contains unauthorized birth or publication evidence"
            ),
            NativeLaunchPhase::Armed => anyhow::ensure!(
                launch.incarnation.is_some()
                    && launch
                        .profile_identity
                        .is_some_and(|identity| identity.is_durable())
                    && launch
                        .stop_endpoint
                        .is_none_or(|endpoint| endpoint.is_durable())
                    && launch.registry.as_ref().is_none_or(|witness| {
                        launch.stop_endpoint.is_some()
                            && witness.record_file_identity.is_durable()
                            && witness.control_file_identity.is_durable()
                    }),
                "Armed launch lacks its original birth or has incomplete publication evidence"
            ),
            NativeLaunchPhase::Published => anyhow::ensure!(
                launch.is_published(),
                "Published launch lacks its actual original publication witnesses"
            ),
            NativeLaunchPhase::Unresolved { .. } => {}
        }
    }
    Ok(())
}

fn history_preserves_originals(before: &[RunnerLaunch], after: &[RunnerLaunch]) -> bool {
    validate_launch_snapshots(before).is_ok()
        && validate_launch_snapshots(after).is_ok()
        && before.len() == after.len()
        && before.iter().all(|original| {
            after
                .iter()
                .any(|current| original.preserves_original_evidence(current))
        })
}

/// Native birth stays immutable; this is the current authorized JSON-file
/// witness and the independently owned original control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RegistryWitness {
    record_file_identity: super::DirectoryIdentity,
    control_file_identity: SocketEndpointIdentity,
    socket_path: PathBuf,
}

impl RegistryWitness {
    fn matches_record(&self, record: &crate::process::worker_registry::WorkerRecord) -> bool {
        self.record_file_identity.is_durable()
            && self.control_file_identity.is_durable()
            && record.record_file_identity == Some(self.record_file_identity)
            && record.control_file_identity == Some(self.control_file_identity)
            && record.socket_path == self.socket_path
    }
}

impl RunnerLaunch {
    fn matches_birth(&self, record: &crate::process::worker_registry::WorkerRecord) -> bool {
        let Some(incarnation) = self.incarnation else {
            return false;
        };
        crate::acp::runner_lifecycle::RunnerIdentity {
            pid: incarnation.pid,
            generation: self.generation,
            launch_nonce: Some(Uuid::from_bytes(self.nonce)),
            incarnation: Some(incarnation),
            profile_identity: self.profile_identity,
            boot: Some(self.boot),
        }
        .matches_record(record)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "coverage", rename_all = "snake_case")]
enum Coverage {
    Complete,
    Unknown { boot: Option<BootToken> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RunnerPreparation {
    nonce: [u8; 16],
    boot: BootToken,
    generation: u64,
}

#[derive(Debug)]
enum PreparationAcknowledgement {
    Produced(std::sync::Arc<LaunchOrigin>),
    Stopped(std::sync::Arc<OwnedStop>),
    NoTarget(std::sync::Arc<OriginalNoTargetRemovalAck>),
}

#[derive(Debug)]
pub(crate) struct PreparationCustody {
    pub(crate) nonce: [u8; 16],
    _completion: std::sync::mpsc::Sender<PreparationAcknowledgement>,
    retired: tokio::sync::watch::Receiver<Option<bool>>,
}

impl PreparationCustody {
    pub(crate) fn retirement(&self) -> &tokio::sync::watch::Receiver<Option<bool>> {
        &self.retired
    }
    pub(crate) fn produced(&self, origin: std::sync::Arc<LaunchOrigin>) -> Result<()> {
        self._completion
            .send(PreparationAcknowledgement::Produced(origin))
            .map_err(|_| anyhow::anyhow!("owned preparation retirement lost its output receiver"))
    }

    pub(crate) fn stopped(&self, stop: std::sync::Arc<OwnedStop>) -> Result<()> {
        self._completion
            .send(PreparationAcknowledgement::Stopped(stop))
            .map_err(|_| anyhow::anyhow!("owned preparation retirement lost its Stop receiver"))
    }

    pub(crate) async fn await_retired(
        mut retirement: tokio::sync::watch::Receiver<Option<bool>>,
    ) -> Result<()> {
        loop {
            let completed = *retirement.borrow_and_update();
            match completed {
                Some(true) => return Ok(()),
                Some(false) => anyhow::bail!("owned preparation completion remains unproven"),
                None => retirement
                    .changed()
                    .await
                    .context("owned preparation completion channel closed without proof")?,
            }
        }
    }

    pub(crate) fn acknowledged_no_target(
        &self,
        acknowledgement: std::sync::Arc<OriginalNoTargetRemovalAck>,
    ) -> Result<()> {
        self._completion
            .send(PreparationAcknowledgement::NoTarget(acknowledgement))
            .map_err(|_| {
                anyhow::anyhow!("original no-target ACK lost its preparation retirement receiver")
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreationCoverage {
    Unknown,
    Owned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunnerExecutionJournal {
    #[serde(flatten)]
    coverage: Coverage,
    launches: Vec<RunnerLaunch>,
    preparations: Vec<RunnerPreparation>,
    creations: Vec<native_create::CreateExecution>,
    create_coverage: CreationCoverage,
}

impl RunnerExecutionJournal {
    pub(crate) fn new() -> Self {
        Self {
            coverage: Coverage::Complete,
            launches: Vec::new(),
            preparations: Vec::new(),
            creations: Vec::new(),
            create_coverage: CreationCoverage::Owned,
        }
    }

    pub(crate) fn legacy_unknown() -> Self {
        Self {
            coverage: Coverage::Unknown { boot: None },
            launches: Vec::new(),
            preparations: Vec::new(),
            creations: Vec::new(),
            create_coverage: CreationCoverage::Unknown,
        }
    }

    pub(crate) fn has_unknown_runner_coverage(&self) -> bool {
        matches!(self.coverage, Coverage::Unknown { .. })
    }

    fn launches(&self) -> &[RunnerLaunch] {
        &self.launches
    }

    fn launches_mut(&mut self) -> &mut Vec<RunnerLaunch> {
        &mut self.launches
    }

    pub(crate) fn proves_quiescent(&self) -> bool {
        self.create_coverage == CreationCoverage::Owned
            && self.creations.iter().all(|record| record.proves_retired())
            && self.proves_runner_quiescent()
    }

    /// Runner admission/stop is not destructive Create/container retirement.
    pub(crate) fn proves_runner_quiescent(&self) -> bool {
        self.proves_for(None)
    }
    pub(crate) fn owns_record(
        &self,
        record: &crate::process::worker_registry::WorkerRecord,
    ) -> bool {
        validate_launch_snapshots(&self.launches).is_ok()
            && self.launches.iter().any(|launch| {
                launch.is_published()
                    && launch.matches_birth(record)
                    && launch
                        .registry
                        .as_ref()
                        .is_some_and(|witness| witness.matches_record(record))
            })
    }

    fn proves_for(&self, nonce: Option<[u8; 16]>) -> bool {
        nonce.is_none()
            && matches!(self.coverage, Coverage::Complete)
            && self.preparations.is_empty()
            && self.launches.is_empty()
    }

    pub(crate) fn has_unresolved_native_launches(&self) -> bool {
        self.launches
            .iter()
            .any(|launch| matches!(launch.phase, NativeLaunchPhase::Unresolved { .. }))
    }
}

/// Revalidate an original execution snapshot before a deferred physical effect.
pub(crate) fn with_current_execution_row<T>(
    storage: &Storage,
    id: &str,
    generation: u64,
    execution: Option<&super::instance::ActiveExecution>,
    effect: impl FnOnce(&Instance) -> Result<T>,
) -> Result<T> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(storage, id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("deferred execution owner disappeared")?;
    anyhow::ensure!(
        row.lifecycle_generation == generation && row.active_execution.as_ref() == execution,
        "deferred execution was superseded"
    );
    effect(&row)
}

fn ensure_unique_owner(storage: &Storage, id: &str) -> Result<()> {
    super::retained_intents::ensure_id_available(id)?;
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
    anyhow::ensure!(
        !row.has_active_lifecycle_reservation(chrono::Utc::now())
            && !row
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.op == LifecycleOperation::Stop),
        "session has an unfinished lifecycle reservation"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("session {0:?} no longer exists")]
pub(crate) struct LaunchSessionGone(pub String);

/// A borrowed, single-use mutation of the already validated original row.
pub(crate) struct PreparationCommit<'a> {
    row: &'a mut Instance,
    ticket: RunnerPreparation,
    admission: &'a crate::acp::runner_lifecycle::ExecutionAdmission,
    prepared: std::sync::Arc<LaunchOrigin>,
    execution: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
}

impl PreparationCommit<'_> {
    pub(crate) fn belongs_to(
        &self,
        admission: &crate::acp::runner_lifecycle::ExecutionAdmission,
    ) -> bool {
        self.admission.same_owner(admission)
    }

    pub(crate) fn commit(self, authorize: impl FnOnce() -> Result<()>) -> Result<()> {
        self.admission
            .commit_preparation(self.prepared, self.execution, || {
                authorize()?;
                self.row.lifecycle_generation = self.ticket.generation;
                self.row.runner_journal.preparations.push(self.ticket);
                Ok(())
            })
    }
}

fn prepare_locked<'a>(
    origin: &LaunchOrigin,
    operation: &crate::acp::runner_lifecycle::NativeResume,
    admission: &crate::acp::runner_lifecycle::ExecutionAdmission,
    commit: impl FnOnce(
        PreparationCommit<'_>,
    ) -> Result<crate::acp::runner_lifecycle::PreparationAuthorization<'a>>,
) -> Result<(std::sync::Arc<LaunchOrigin>, PreparationCustody)> {
    let storage = origin.storage();
    let id = origin.session_id();
    let generation = origin
        .generation()
        .checked_add(1)
        .context("lifecycle generation overflow")?;
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let prepared = std::sync::Arc::new(LaunchOrigin {
        plan: origin.plan.clone(),
        generation,
        births: origin.births.clone(),
        creations: origin.creations.clone(),
        create_coverage: origin.create_coverage,
    });
    let mut retirement_scope = prepared.clone();
    let mut retirement_stop = None;
    let mut no_target_acknowledgement = None;
    let nonce = *Uuid::new_v4().as_bytes();
    let (completion, finished) = std::sync::mpsc::channel();
    let (retired_tx, retired) = tokio::sync::watch::channel(None);
    let original = storage.clone();
    let session_id = id.to_owned();
    std::thread::Builder::new().name("aoe-preparation-retirement".into()).spawn(move || {
        for acknowledgement in finished {
            match acknowledgement {
                PreparationAcknowledgement::Produced(produced) => retirement_scope = produced,
                PreparationAcknowledgement::Stopped(stop) => retirement_stop = Some(stop),
                PreparationAcknowledgement::NoTarget(acknowledgement) => {
                    no_target_acknowledgement = Some(acknowledgement);
                    break;
                }
            }
        }
        let result = (|| -> Result<()> {
            let _workspace = super::acquire_session_workspace_claim_lock()?;
            let _identity = super::acquire_session_identity_lock()?;
            original.verify_profile_identity()?;
            let _lifecycle = original.acquire_instance_lifecycle_lock(&session_id)?;
            let acknowledgement = no_target_acknowledgement
                .context("preparation completion lacks an original no-target producer ACK; retaining protection")?;
            let acknowledgement = retirement_scope.with_acknowledged_no_target_retirement(acknowledgement)?;
            retirement_scope = acknowledgement.derived().clone();
            let validate = |row: &Instance| -> Result<()> {
                if let Some(stop) = &retirement_stop {
                    anyhow::ensure!(std::sync::Arc::ptr_eq(&stop.native_origin(), &retirement_scope),
                        "preparation Stop has not consumed its original no-target ACK");
                    stop.current_projection().validate_baseline_at(row, stop.generation())?;
                    stop.validate_acknowledged_native_history(row)?;
                    anyhow::ensure!(row.lifecycle_reservation_is_owned(stop.operation(), stop.generation()),
                        "preparation Stop lost its original claim");
                    Ok(())
                } else {
                    retirement_scope.validate_baseline_at(row, row.lifecycle_generation)
                }
            };
            let removed = original.update_under_workspace_claim_lock(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == session_id)
                    .context("original preparation owner disappeared without its durable ACK")?;
                validate(row)?;
                let count = row.runner_journal.preparations.iter().filter(|ticket|
                    ticket.nonce == nonce && ticket.boot == boot && ticket.generation == generation).count();
                anyhow::ensure!(count == 1, "original preparation ACK has missing or duplicate tuples");
                row.runner_journal.preparations.retain(|ticket|
                    ticket.nonce != nonce || ticket.boot != boot || ticket.generation != generation);
                anyhow::ensure!(row.runner_journal.proves_runner_quiescent(),
                    "preparation no-target ACK left another protected runner domain");
                Ok(())
            }).and_then(|_| sync_parent_directory(original.sessions_path()));
            if let Err(error) = removed {
                original.update_under_workspace_claim_lock(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == session_id)
                        .context("original preparation owner disappeared during uncertain ACK retention")?;
                    validate(row)?;
                    if !row.runner_journal.preparations.iter().any(|ticket|
                        ticket.nonce == nonce && ticket.boot == boot && ticket.generation == generation) {
                        row.runner_journal.preparations.push(RunnerPreparation { nonce, boot, generation });
                    }
                    Ok(())
                }).and_then(|_| sync_parent_directory(original.sessions_path()))
                    .context("retaining original preparation after uncertain durable ACK")?;
                return Err(error);
            }
            let row = original.load_strict_for_worktree_ownership_locked()?.into_iter()
                .find(|row| row.id == session_id)
                .context("original preparation writer lost its acknowledged row")?;
            validate(&row)?;
            anyhow::ensure!(!row.runner_journal.preparations.iter().any(|ticket|
                ticket.nonce == nonce && ticket.boot == boot && ticket.generation == generation),
                "original preparation writer did not acknowledge exact tuple removal");
            anyhow::ensure!(row.runner_journal.proves_runner_quiescent(),
                "original preparation writer still retains protected runner authority");
            Ok(())
        })();
        let _ = retired_tx.send(Some(result.is_ok()));
        if let Err(error) = result {
            tracing::warn!(target: "acp.supervisor", session = %session_id, %error, "preparation completion remains unproven");
        }
    })?;
    let custody = PreparationCustody {
        nonce,
        _completion: completion,
        retired,
    };
    admission.register_preparation_retirement(custody.retirement().clone())?;
    let authorization = storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or_else(|| LaunchSessionGone(id.to_owned()))?;
        origin.validate_row(row)?;
        anyhow::ensure!(row.runner_journal.preparations.is_empty(),
            "session already has unfinished launch preparation");
        match operation {
            crate::acp::runner_lifecycle::NativeResume::Spawn => {
                anyhow::ensure!(row.runner_journal.proves_runner_quiescent(),
                    "runner history does not prove quiescence before launch preparation");
                if let Some(record) = crate::process::worker_registry::load_strict(id)? {
                    retire_quiescent_record_locked(storage, &record, &row.runner_journal, None)?;
                }
            }
            crate::acp::runner_lifecycle::NativeResume::Attach(captured) => {
                let record = crate::process::worker_registry::load_strict(id)?
                    .context("resident runner disappeared before attach preparation")?;
                anyhow::ensure!(&record == captured.as_ref(),
                    "resident registry birth or borrowed file was superseded before attach preparation");
                let incarnation = record.incarnation.context("resident runner has no native birth proof")?;
                anyhow::ensure!(record.boot == Some(boot)
                    && record.profile_identity == Some(storage.original_profile_identity()?)
                    && incarnation.pid == record.pid && incarnation.group == record.pid
                    && crate::process::process_incarnation(record.pid)? == Some(incarnation)
                    && crate::process::worker::is_process_group_alive(record.pid)
                    && row.runner_journal.launches().iter().any(|launch| launch.is_published() && launch.matches_birth(&record)
                        && launch.registry.as_ref().is_some_and(|witness| witness.matches_record(&record))),
                    "resident runner lacks its exact original published birth");
            }
        }
        let execution = match operation {
            crate::acp::runner_lifecycle::NativeResume::Spawn => None,
            crate::acp::runner_lifecycle::NativeResume::Attach(record) => Some(crate::acp::runner_lifecycle::RunnerIdentity {
                pid: record.pid, generation: record.generation, launch_nonce: record.launch_nonce,
                incarnation: record.incarnation, profile_identity: record.profile_identity, boot: record.boot }),
        };
        commit(PreparationCommit { row, ticket: RunnerPreparation { nonce, boot, generation }, admission, prepared: prepared.clone(), execution })
    })?;
    sync_parent_directory(storage.sessions_path())?;
    authorization.commit();
    Ok((prepared, custody))
}

/// Physical original and immutable execution plan shared by sealed issued epochs.
struct LaunchPlan {
    storage: std::sync::Arc<Storage>,
    execution: ExecutionPlan,
    pre_trash_project_path: Option<String>,
    scratch: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct ExecutionPlan {
    session_id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    project_path: String,
    worktree: Option<super::WorktreeInfo>,
    workspace: Option<super::WorkspaceInfo>,
    sandbox: Option<super::SandboxInfo>,
    command: String,
    extra_args: String,
    tool: String,
    detect_as: String,
    yolo_mode: bool,
    agent_provider: Option<String>,
    first_launch_names_agent: bool,
    active_execution: Option<super::instance::ActiveExecution>,
    title: String,
    archived: bool,
    trashed: bool,
}

impl std::ops::Deref for LaunchPlan {
    type Target = ExecutionPlan;

    fn deref(&self) -> &Self::Target {
        &self.execution
    }
}

/// One sealed original-profile authority epoch. A derivative cannot promote an old observer.
#[derive(Clone)]
pub struct LaunchOrigin {
    plan: std::sync::Arc<LaunchPlan>,
    generation: u64,
    births: std::sync::Arc<[RunnerLaunch]>,
    creations: std::sync::Arc<[native_create::CreateExecution]>,
    create_coverage: CreationCoverage,
}

#[derive(Debug)]
pub(crate) struct OriginalNoTargetRemovalAck {
    original: std::sync::Arc<LaunchOrigin>,
    launch: RunnerLaunch,
    identity: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    derived: std::sync::Arc<LaunchOrigin>,
}

impl OriginalNoTargetRemovalAck {
    pub(crate) fn original(&self) -> &std::sync::Arc<LaunchOrigin> {
        &self.original
    }

    pub(crate) fn launch(&self) -> &RunnerLaunch {
        &self.launch
    }

    pub(crate) fn identity(&self) -> Option<crate::acp::runner_lifecycle::RunnerIdentity> {
        self.identity
    }

    pub(crate) fn derived(&self) -> &std::sync::Arc<LaunchOrigin> {
        &self.derived
    }
}

impl std::fmt::Debug for LaunchOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOrigin")
            .field("session_id", &self.plan.session_id)
            .field("project_path", &self.plan.project_path)
            .finish_non_exhaustive()
    }
}

pub(crate) fn sandbox_geometry_matches(
    expected: Option<&super::SandboxInfo>,
    current: Option<&super::SandboxInfo>,
) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(expected), Some(current)) => {
            expected.enabled == current.enabled
                && expected.image == current.image
                && expected.container_workdir == current.container_workdir
                && expected.extra_env == current.extra_env
                && expected.custom_instruction == current.custom_instruction
        }
        _ => false,
    }
}

impl LaunchOrigin {
    pub(crate) fn storage(&self) -> &Storage {
        &self.plan.storage
    }

    pub(crate) fn prepare<'a>(
        &self,
        operation: &crate::acp::runner_lifecycle::NativeResume,
        admission: &crate::acp::runner_lifecycle::ExecutionAdmission,
        commit: impl FnOnce(
            PreparationCommit<'_>,
        )
            -> Result<crate::acp::runner_lifecycle::PreparationAuthorization<'a>>,
    ) -> Result<(std::sync::Arc<Self>, PreparationCustody)> {
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        self.plan.storage.verify_profile_identity()?;
        anyhow::ensure!(
            self.plan.storage.original_profile_identity()?.is_durable(),
            "native profile birth time is unavailable; fresh launch is unproven"
        );
        let _lifecycle = self
            .plan
            .storage
            .acquire_instance_lifecycle_lock(&self.plan.session_id)?;
        ensure_unique_owner(&self.plan.storage, &self.plan.session_id)?;
        prepare_locked(self, operation, admission, commit)
    }

    /// Snapshot original plan authority without issuing launch preparation; stop
    /// and queued resume validation never acquire authority by a later name lookup.
    pub(crate) fn capture_baseline(expected: &super::Instance) -> Result<Self> {
        Self::capture_baseline_at(expected, expected.original_storage()?)
    }

    fn capture_baseline_at(expected: &Instance, storage: std::sync::Arc<Storage>) -> Result<Self> {
        super::retained_intents::ensure_id_available(&expected.id)?;
        anyhow::ensure!(
            expected
                .storage_origin
                .as_ref()
                .is_none_or(|original| original.same_origin_as(&storage)),
            "captured row belongs to a different physical original"
        );
        validate_launch_snapshots(expected.runner_journal.launches())?;
        Ok(Self {
            plan: std::sync::Arc::new(LaunchPlan {
                storage,
                pre_trash_project_path: expected.pre_trash_project_path.clone(),
                scratch: expected.scratch,
                execution: ExecutionPlan {
                    session_id: expected.id.clone(),
                    created_at: expected.created_at,
                    project_path: expected.project_path.clone(),
                    worktree: expected.worktree_info.clone(),
                    workspace: expected.workspace_info.clone(),
                    sandbox: expected.sandbox_info.clone(),
                    command: expected.command.clone(),
                    extra_args: expected.extra_args.clone(),
                    tool: expected.tool.clone(),
                    detect_as: expected.detect_as.clone(),
                    yolo_mode: expected.yolo_mode,
                    agent_provider: expected.agent_provider.clone(),
                    first_launch_names_agent: expected.first_launch_names_agent,
                    active_execution: expected.active_execution.clone(),
                    title: expected.title.clone(),
                    archived: expected.is_archived(),
                    trashed: expected.is_trashed(),
                },
            }),
            generation: expected.lifecycle_generation,
            creations: expected.runner_journal.creations.clone().into(),
            create_coverage: expected.runner_journal.create_coverage,
            births: expected.runner_journal.launches().to_vec().into(),
        })
    }

    /// Snapshot the immutable original row. Native admission must claim its
    /// preparation before any launch, backend, or container effect.
    pub fn capture(expected: &super::Instance) -> Result<std::sync::Arc<Self>> {
        Self::capture_baseline(expected).map(std::sync::Arc::new)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn is_prepared_from(&self, baseline: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.plan, &baseline.plan)
            && baseline.generation.checked_add(1) == Some(self.generation)
    }

    pub(crate) fn same_scope(&self, other: &Self) -> bool {
        self.same_scope_at(other, self.generation)
    }

    fn same_scope_at(&self, other: &Self, generation: u64) -> bool {
        self.same_projection_at(other, generation) && self.same_birth_scope(other)
    }

    fn same_projection_at(&self, other: &Self, generation: u64) -> bool {
        let (a, b) = (&self.plan, &other.plan);
        let workspace_matches = match (&a.workspace, &b.workspace) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.workspace_dir == b.workspace_dir && a.branch == b.branch && a.repos == b.repos
            }
            _ => false,
        };
        generation == other.generation
            && a.storage.same_origin_as(&b.storage)
            && a.session_id == b.session_id
            && a.created_at == b.created_at
            && a.title == b.title
            && a.archived == b.archived
            && a.trashed == b.trashed
            && a.pre_trash_project_path == b.pre_trash_project_path
            && a.scratch == b.scratch
            && a.project_path == b.project_path
            && a.worktree == b.worktree
            && workspace_matches
            && sandbox_geometry_matches(a.sandbox.as_ref(), b.sandbox.as_ref())
            && a.command == b.command
            && a.extra_args == b.extra_args
            && a.tool == b.tool
            && a.detect_as == b.detect_as
            && a.yolo_mode == b.yolo_mode
            && a.agent_provider == b.agent_provider
            && a.first_launch_names_agent == b.first_launch_names_agent
            && a.active_execution == b.active_execution
    }

    /// Recognize a stale cache only through an actual producer's retained ACK.
    /// The physical FDA, complete plan, counter, and every previously observed
    /// birth must still match; this does not authorize discovering new history.
    pub(crate) fn recognizes_published_snapshot(&self, cached: &Self) -> bool {
        self.same_projection_at(cached, self.generation)
            && self.same_creation_scope(cached.create_coverage, &cached.creations)
            && validate_launch_snapshots(&self.births).is_ok()
            && validate_launch_snapshots(&cached.births).is_ok()
            && cached.births.iter().all(|before| {
                self.births
                    .iter()
                    .any(|after| before.preserves_original_evidence(after))
            })
    }

    pub(crate) fn recognizes_published_instance(&self, cached: &Instance) -> bool {
        cached
            .storage_origin
            .as_ref()
            .is_some_and(|storage| storage.same_origin_as(&self.plan.storage))
            && self.plan_matches_at(cached, self.generation, self.plan.trashed)
            && self.same_creation_scope(
                cached.runner_journal.create_coverage,
                &cached.runner_journal.creations,
            )
            && validate_launch_snapshots(&self.births).is_ok()
            && validate_launch_snapshots(cached.runner_journal.launches()).is_ok()
            && cached.runner_journal.launches().iter().all(|before| {
                self.births
                    .iter()
                    .any(|after| before.preserves_original_evidence(after))
            })
    }

    fn same_creation_scope(
        &self,
        coverage: CreationCoverage,
        records: &[native_create::CreateExecution],
    ) -> bool {
        self.create_coverage == coverage
            && self.creations.len() == records.len()
            && self
                .creations
                .iter()
                .zip(records)
                .all(|(a, b)| a.same_record(b))
    }

    fn same_birth_scope(&self, other: &Self) -> bool {
        self.same_creation_scope(other.create_coverage, &other.creations)
            && validate_launch_snapshots(&self.births).is_ok()
            && validate_launch_snapshots(&other.births).is_ok()
            && self.births.len() == other.births.len()
            && self
                .births
                .iter()
                .all(|launch| other.births.contains(launch))
    }

    pub(crate) fn profile(&self) -> &str {
        self.plan.storage.profile()
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.plan.session_id
    }

    pub(crate) fn matches_instance(&self, row: &Instance) -> bool {
        row.storage_origin
            .as_ref()
            .is_some_and(|storage| self.plan.storage.same_origin_as(storage))
            && self.validate_baseline_at(row, self.generation).is_ok()
    }

    pub(crate) fn validate_request(
        &self,
        cwd: &std::path::Path,
        tool: &str,
        sandbox: Option<&super::SandboxInfo>,
        yolo_mode: bool,
        command: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        anyhow::ensure!(
            std::path::Path::new(&self.plan.project_path) == cwd
                && self.plan.tool == tool
                && self.plan.yolo_mode == yolo_mode
                && self.plan.agent_provider.as_deref() == provider
                && sandbox_geometry_matches(
                    self.plan.sandbox.as_ref().filter(|sandbox| sandbox.enabled),
                    sandbox.filter(|sandbox| sandbox.enabled)
                )
                && command.unwrap_or_default().trim() == self.plan.command.trim(),
            "launch request does not match its original stored execution plan"
        );
        Ok(())
    }

    pub(crate) fn validate_baseline_at(&self, row: &Instance, generation: u64) -> Result<()> {
        self.validate_plan_at(row, generation, self.plan.trashed)?;
        self.validate_native_history(row)
    }

    pub(crate) fn validate_restored_at(&self, row: &Instance, generation: u64) -> Result<()> {
        anyhow::ensure!(self.plan.trashed, "original instance was not trashed");
        self.validate_plan_at(row, generation, false)?;
        self.validate_native_history(row)
    }

    fn plan_matches_at(&self, row: &Instance, generation: u64, trashed: bool) -> bool {
        let workspace_matches = match (&self.plan.workspace, &row.workspace_info) {
            (None, None) => true,
            (Some(expected), Some(current)) => {
                expected.workspace_dir == current.workspace_dir
                    && expected.branch == current.branch
                    && expected.repos == current.repos
            }
            _ => false,
        };
        let sandbox_matches =
            sandbox_geometry_matches(self.plan.sandbox.as_ref(), row.sandbox_info.as_ref());
        row.id == self.plan.session_id
            && row.created_at == self.plan.created_at
            && row.title == self.plan.title
            && row.is_archived() == self.plan.archived
            && row.is_trashed() == trashed
            && row.pre_trash_project_path == self.plan.pre_trash_project_path
            && row.scratch == self.plan.scratch
            && row.project_path == self.plan.project_path
            && row.worktree_info == self.plan.worktree
            && workspace_matches
            && sandbox_matches
            && row.command == self.plan.command
            && row.extra_args == self.plan.extra_args
            && row.tool == self.plan.tool
            && row.detect_as == self.plan.detect_as
            && row.yolo_mode == self.plan.yolo_mode
            && row.agent_provider == self.plan.agent_provider
            && row.first_launch_names_agent == self.plan.first_launch_names_agent
            && row.active_execution == self.plan.active_execution
            && row.lifecycle_generation == generation
    }

    fn validate_plan_at(&self, row: &Instance, generation: u64, trashed: bool) -> Result<()> {
        super::retained_intents::ensure_id_available(&self.plan.session_id)?;
        anyhow::ensure!(
            self.plan_matches_at(row, generation, trashed),
            "original lifecycle or execution plan was superseded"
        );
        Ok(())
    }

    pub(super) fn validate_native_history(&self, row: &Instance) -> Result<()> {
        anyhow::ensure!(
            self.same_creation_scope(
                row.runner_journal.create_coverage,
                &row.runner_journal.creations
            ),
            "original Create native scope changed without its producer ACK"
        );
        anyhow::ensure!(
    history_preserves_originals(&self.births, row.runner_journal.launches()),
    "original native history disappeared, rebound, grew, or weakened without its producer evidence"
);
        Ok(())
    }

    pub(crate) fn with_issued_birth(
        &self,
        identity: crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> Result<std::sync::Arc<Self>> {
        anyhow::ensure!(
            identity.birth_is_complete(),
            "issued native birth is incomplete"
        );
        anyhow::ensure!(
            identity.generation == self.generation
                && identity.profile_identity == Some(self.storage().original_profile_identity()?),
            "issued native birth does not belong to this prepared authority"
        );
        validate_launch_snapshots(&self.births)?;
        let nonce = *identity
            .launch_nonce
            .context("issued native nonce is absent")?
            .as_bytes();
        let boot = identity.boot.context("issued native boot is absent")?;
        let mut indices = self.births.iter().enumerate().filter(|(_, launch)| {
            launch.nonce == nonce
                && launch.boot == boot
                && launch.generation == identity.generation
                && launch.profile_identity == identity.profile_identity
        });
        let index = indices
            .next()
            .map(|(index, _)| index)
            .context("issued birth has no original Natal slot")?;
        anyhow::ensure!(
            indices.next().is_none(),
            "issued birth has duplicate Natal slots"
        );
        let prior = &self.births[index];
        anyhow::ensure!(
            prior.phase == NativeLaunchPhase::Natal
                && prior.incarnation.is_none()
                && prior.stop_endpoint.is_none()
                && prior.registry.is_none(),
            "issued native birth replaced an already armed or uncertain original"
        );
        let mut births = self.births.to_vec();
        births[index].incarnation = identity.incarnation;
        births[index].phase = NativeLaunchPhase::Armed;
        validate_launch_snapshots(&births)?;
        Ok(std::sync::Arc::new(Self {
            plan: self.plan.clone(),
            generation: self.generation,
            births: births.into(),
            creations: self.creations.clone(),
            create_coverage: self.create_coverage,
        }))
    }

    pub(super) fn with_launch_snapshot(
        &self,
        launch: RunnerLaunch,
    ) -> Result<std::sync::Arc<Self>> {
        validate_launch_snapshots(&self.births)?;
        validate_launch_snapshots(std::slice::from_ref(&launch))?;
        let mut indices = self.births.iter().enumerate().filter(|(_, before)| {
            before.nonce == launch.nonce
                && before.boot == launch.boot
                && before.generation == launch.generation
                && before.profile_identity == launch.profile_identity
        });
        let index = indices
            .next()
            .map(|(index, _)| index)
            .context("producer snapshot lacks its original issued birth")?;
        anyhow::ensure!(
            indices.next().is_none(),
            "producer snapshot has duplicate original births"
        );
        anyhow::ensure!(
            self.births[index].preserves_original_evidence(&launch),
            "producer snapshot weakened original launch phase or resource evidence"
        );
        let mut births = self.births.to_vec();
        births[index] = launch;
        Ok(std::sync::Arc::new(Self {
            plan: self.plan.clone(),
            generation: self.generation,
            births: births.into(),
            creations: self.creations.clone(),
            create_coverage: self.create_coverage,
        }))
    }

    pub(crate) fn is_output_from(&self, expected: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.plan, &expected.plan)
            && self.generation == expected.generation
            && validate_launch_snapshots(&self.births).is_ok()
            && validate_launch_snapshots(&expected.births).is_ok()
            && expected.births.iter().all(|before| {
                self.births
                    .iter()
                    .any(|after| before.preserves_original_evidence(after))
            })
    }

    fn with_pending_birth(&self, nonce: [u8; 16], boot: BootToken) -> Result<std::sync::Arc<Self>> {
        validate_launch_snapshots(&self.births)?;
        anyhow::ensure!(
            !self.births.iter().any(|launch| launch.nonce == nonce
                && launch.boot == boot
                && launch.generation == self.generation),
            "pending native slot already exists"
        );
        let mut births = Vec::with_capacity(self.births.len() + 1);
        births.extend(self.births.iter().cloned());
        births.push(RunnerLaunch {
            nonce,
            boot,
            generation: self.generation,
            incarnation: None,
            profile_identity: Some(self.storage().original_profile_identity()?),
            phase: NativeLaunchPhase::Natal,
            stop_endpoint: None,
            registry: None,
        });
        Ok(std::sync::Arc::new(Self {
            plan: self.plan.clone(),
            generation: self.generation,
            births: births.into(),
            creations: self.creations.clone(),
            create_coverage: self.create_coverage,
        }))
    }

    pub(crate) fn validate_record_birth(
        &self,
        record: &crate::process::worker_registry::WorkerRecord,
    ) -> Result<()> {
        validate_launch_snapshots(&self.births)?;
        let key = NativeBirthKey {
            nonce: *record
                .launch_nonce
                .context("resident native nonce is unknown")?
                .as_bytes(),
            boot: record.boot.context("resident native boot is unknown")?,
            generation: record.generation,
            incarnation: record.incarnation,
            profile_identity: record.profile_identity,
        };
        anyhow::ensure!(
            record
                .incarnation
                .is_some_and(|incarnation| incarnation.pid == record.pid)
                && self.births.iter().any(|launch| launch.birth_key() == key),
            "resident record is not an originally captured native birth"
        );
        Ok(())
    }

    fn validate_row(&self, row: &Instance) -> Result<()> {
        anyhow::ensure!(
            row.id == self.plan.session_id && row.created_at == self.plan.created_at,
            "original instance was replaced"
        );
        startable(row)?;
        self.validate_baseline_at(row, self.generation)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.with_storage(|_, _row| Ok(()))
    }

    pub(crate) fn with_storage<T>(
        &self,
        effect: impl FnOnce(&Storage, Instance) -> Result<T>,
    ) -> Result<T> {
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        self.plan.storage.verify_profile_identity()?;
        let _lifecycle = self
            .plan
            .storage
            .acquire_instance_lifecycle_lock(&self.plan.session_id)?;
        ensure_unique_owner(&self.plan.storage, &self.plan.session_id)?;
        let row = self
            .plan
            .storage
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|row| row.id == self.plan.session_id)
            .ok_or_else(|| LaunchSessionGone(self.plan.session_id.clone()))?;
        self.validate_row(&row)?;
        effect(&self.plan.storage, row)
    }
    /// Commit the original row, then publish its result under the same physical fences.
    pub(crate) fn update_storage<T, U>(
        &self,
        effect: impl FnOnce(&Storage, &mut Instance) -> Result<T>,
        publish: impl FnOnce(T) -> Result<U>,
    ) -> Result<U> {
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        self.plan.storage.verify_profile_identity()?;
        let _lifecycle = self
            .plan
            .storage
            .acquire_instance_lifecycle_lock(&self.plan.session_id)?;
        ensure_unique_owner(&self.plan.storage, &self.plan.session_id)?;
        self.plan
            .storage
            .update_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.plan.session_id)
                    .ok_or_else(|| LaunchSessionGone(self.plan.session_id.clone()))?;
                self.validate_row(row)?;
                effect(&self.plan.storage, row)
            })
            .and_then(publish)
    }

    fn no_target_native_key(
        &self,
        launch: &RunnerLaunch,
        identity: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    ) -> Result<NativeBirthKey> {
        validate_launch_snapshots(&self.births)?;
        validate_launch_snapshots(std::slice::from_ref(launch))?;
        anyhow::ensure!(
            launch.generation == self.generation
                && launch.profile_identity == Some(self.storage().original_profile_identity()?),
            "no-target ACK changed the physical original or execution epoch"
        );
        let key = launch.birth_key();
        let mut originals = self
            .births
            .iter()
            .filter(|before| before.birth_key() == key);
        let before = originals
            .next()
            .context("no-target ACK lacks its original captured launch slot")?;
        anyhow::ensure!(
            originals.next().is_none(),
            "no-target ACK has ambiguous original launch slots"
        );
        anyhow::ensure!(
            before.preserves_original_evidence(launch)
                && matches!(
                    launch.phase,
                    NativeLaunchPhase::Natal
                        | NativeLaunchPhase::Armed
                        | NativeLaunchPhase::Unresolved {
                            may_authorize: false
                        }
                ),
            "no-target ACK weakened its original evidence or includes possibly-authorized work"
        );
        match identity {
            Some(identity) => {
                anyhow::ensure!(
                    identity.birth_is_complete()
                        && identity.launch_nonce.map(|nonce| *nonce.as_bytes()) == Some(key.nonce)
                        && identity.boot == Some(key.boot)
                        && identity.generation == key.generation
                        && identity.profile_identity == key.profile_identity
                        && (identity.incarnation == key.incarnation
                            || key.incarnation.is_none()
                                && matches!(
                                    launch.phase,
                                    NativeLaunchPhase::Natal
                                        | NativeLaunchPhase::Unresolved {
                                            may_authorize: false
                                        }
                                )),
                    "no-target ACK root is not its exact original born or pending slot"
                );
            }
            None => anyhow::ensure!(
                matches!(
                    launch.phase,
                    NativeLaunchPhase::Natal
                        | NativeLaunchPhase::Unresolved {
                            may_authorize: false
                        }
                ) && launch.incarnation.is_none()
                    && launch.stop_endpoint.is_none()
                    && launch.registry.is_none(),
                "sealed pending-slot no-target ACK is not its original unpublished tuple"
            ),
        }
        Ok(key)
    }

    fn derive_no_target_successor(
        &self,
        launch: &RunnerLaunch,
        identity: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    ) -> Result<std::sync::Arc<Self>> {
        let key = self.no_target_native_key(launch, identity)?;
        let mut births = Vec::with_capacity(self.births.len() - 1);
        births.extend(
            self.births
                .iter()
                .filter(|launch| launch.birth_key() != key)
                .cloned(),
        );
        Ok(std::sync::Arc::new(Self {
            plan: self.plan.clone(),
            generation: self.generation,
            births: births.into(),
            creations: self.creations.clone(),
            create_coverage: self.create_coverage,
        }))
    }

    pub(crate) fn with_acknowledged_no_target_retirement(
        self: &std::sync::Arc<Self>,
        acknowledgement: std::sync::Arc<OriginalNoTargetRemovalAck>,
    ) -> Result<std::sync::Arc<OriginalNoTargetRemovalAck>> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(self, acknowledgement.original()),
            "no-target ACK belongs to another original authority"
        );
        let key =
            self.no_target_native_key(acknowledgement.launch(), acknowledgement.identity())?;
        let derived = acknowledgement.derived();
        validate_launch_snapshots(&derived.births)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.plan, &derived.plan)
                && self.generation == derived.generation
                && self.same_creation_scope(derived.create_coverage, &derived.creations)
                && derived.births.len().checked_add(1) == Some(self.births.len())
                && self
                    .births
                    .iter()
                    .filter(|launch| launch.birth_key() != key)
                    .all(|launch| derived.births.contains(launch))
                && !derived
                    .births
                    .iter()
                    .any(|launch| launch.birth_key() == key),
            "acknowledged no-target derivative changed an unrelated goal, epoch, birth or resource"
        );
        Ok(acknowledgement)
    }
}

/// An owned Stop receipt: immutable original plan plus its single committed transition.
/// Clones keep cancellation cleanup from releasing the claim ahead of an owned driver.
#[derive(Debug)]
pub(crate) struct OwnedStop {
    original: std::sync::Arc<LaunchOrigin>,
    generation: u64,
    operation: LifecycleOperation,
    finished: std::sync::atomic::AtomicBool,
    native_acknowledged: std::sync::Mutex<Option<std::sync::Arc<OriginalNoTargetRemovalAck>>>,
    acknowledged: std::sync::Mutex<Option<std::sync::Arc<LaunchOrigin>>>,
    native_create: native_create::CreateNativeCustody,
}

impl OwnedStop {
    pub(crate) fn storage(&self) -> &Storage {
        self.original.storage()
    }
    pub(crate) fn session_id(&self) -> &str {
        self.original.session_id()
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn operation(&self) -> LifecycleOperation {
        self.operation
    }

    /// Borrow the operation already emitted by its durable writer; never mint another Stop.
    pub(crate) fn from_claim(
        storage: &Storage,
        row: &Instance,
        operation: LifecycleOperation,
        generation: u64,
    ) -> Result<std::sync::Arc<Self>> {
        anyhow::ensure!(
            row.lifecycle_generation == generation
                && row.lifecycle_reservation_is_owned(operation, generation),
            "borrowed native claim lost its original operation"
        );
        storage.verify_profile_identity()?;
        let original =
            LaunchOrigin::capture_baseline_at(row, std::sync::Arc::new(storage.clone()))?;
        Ok(Self::new(
            std::sync::Arc::new(original),
            generation,
            operation,
        ))
    }
    pub(crate) fn original_arc(&self) -> std::sync::Arc<LaunchOrigin> {
        self.original.clone()
    }

    pub(crate) fn current_projection(&self) -> std::sync::Arc<LaunchOrigin> {
        self.acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.original.clone())
    }

    fn new(
        original: std::sync::Arc<LaunchOrigin>,
        generation: u64,
        operation: LifecycleOperation,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            original,
            generation,
            operation,
            native_acknowledged: Default::default(),
            acknowledged: std::sync::Mutex::new(None),
            native_create: Default::default(),
            finished: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Commit and publish a metadata ACK without changing the original native births.
    pub(crate) fn update_projection<T, U>(
        &self,
        effect: impl FnOnce(&mut Instance) -> Result<T>,
        publish: impl FnOnce(T) -> Result<U>,
    ) -> Result<U> {
        let storage = self.storage();
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        storage.verify_profile_identity()?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock(self.session_id())?;
        ensure_unique_owner(storage, self.session_id())?;
        let expected = self.current_projection();
        let mut emitted = None;
        let result = storage.update_under_workspace_claim_lock(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == self.session_id())
                .context("original projection row disappeared")?;
            expected.validate_baseline_at(row, self.generation)?;
            anyhow::ensure!(
                row.lifecycle_reservation_is_owned(self.operation, self.generation),
                "projection writer lost its original claim"
            );
            let reservation = row.lifecycle_reservation.clone();
            let journal = row.runner_journal.clone();
            let result = effect(row)?;
            anyhow::ensure!(
                row.id == self.session_id()
                    && row.created_at == self.original.plan.created_at
                    && row.lifecycle_generation == self.generation
                    && row.lifecycle_reservation.as_ref() == reservation.as_ref()
                    && row.lifecycle_reservation_is_owned(self.operation, self.generation)
                    && row.runner_journal == journal,
                "metadata projection changed the original row, claim, or native evidence"
            );
            row.storage_origin = Some(self.original.plan.storage.clone());
            let mut output = LaunchOrigin::capture_baseline(row)?;
            output.births = self.native_origin().births.clone();
            emitted = Some(std::sync::Arc::new(output));
            Ok(result)
        })?;
        sync_parent_directory(storage.sessions_path())?;
        *self
            .acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = emitted;
        publish(result)
    }

    /// The caller retains the real PurgeTransaction for the whole driver.
    pub(crate) fn from_purge(
        original: std::sync::Arc<LaunchOrigin>,
        generation: u64,
    ) -> std::sync::Arc<Self> {
        Self::new(original, generation, LifecycleOperation::Purge)
    }

    /// Retain the actual ACK of the original Attach reservation.
    pub(crate) fn from_attach(
        original: std::sync::Arc<LaunchOrigin>,
        acknowledged: std::sync::Arc<LaunchOrigin>,
    ) -> Result<std::sync::Arc<Self>> {
        anyhow::ensure!(
            original.same_scope_at(&acknowledged, acknowledged.generation),
            "attach ACK changed its original complete native scope"
        );
        let scope = Self::new(
            original,
            acknowledged.generation,
            LifecycleOperation::Attach,
        );
        *scope
            .acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(acknowledged);
        Ok(scope)
    }
    pub(crate) fn original(&self) -> &LaunchOrigin {
        &self.original
    }

    // Metadata ACK matching only; native effects still require the original scope.
    pub(crate) fn acknowledges_original_epoch(
        &self,
        row: &Instance,
        original_generation: u64,
    ) -> bool {
        self.original.generation() == original_generation
            && original_generation.checked_add(1) == Some(self.generation)
            && row.id == self.session_id()
            && row.created_at == self.original.plan.created_at
            && row.lifecycle_generation == self.generation
            && row
                .storage_origin
                .as_ref()
                .is_some_and(|origin| self.storage().same_origin_as(origin))
            && self.validate_acknowledged_native_history(row).is_ok()
    }

    pub(crate) fn cancellation_origin(&self) -> std::sync::Arc<LaunchOrigin> {
        let projection = self.current_projection();
        let native = self.native_origin();
        if projection.generation() == self.generation {
            return projection;
        }
        std::sync::Arc::new(LaunchOrigin {
            plan: projection.plan.clone(),
            creations: projection.creations.clone(),
            create_coverage: native.create_coverage,
            generation: self.generation,
            births: native.births.clone(),
        })
    }

    pub(crate) fn with_scope<T>(&self, effect: impl FnOnce(&Instance) -> Result<T>) -> Result<T> {
        let storage = self.storage();
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        storage.verify_profile_identity()?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock(self.session_id())?;
        ensure_unique_owner(storage, self.session_id())?;
        let row = storage
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|row| row.id == self.session_id())
            .context("original session disappeared during stop")?;
        self.current_projection()
            .validate_baseline_at(&row, self.generation)?;
        if self.operation != LifecycleOperation::Create {
            self.validate_acknowledged_native_history(&row)?;
        }
        anyhow::ensure!(
            row.lifecycle_reservation_is_owned(self.operation, self.generation),
            "original stop claim was superseded"
        );
        effect(&row)
    }

    fn native_origin(&self) -> std::sync::Arc<LaunchOrigin> {
        self.native_acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|acknowledgement| acknowledgement.derived().clone())
            .unwrap_or_else(|| self.original.clone())
    }

    pub(crate) fn acknowledge_no_target_retirement(
        &self,
        acknowledgement: std::sync::Arc<OriginalNoTargetRemovalAck>,
    ) -> Result<()> {
        let acknowledgement = self
            .original
            .with_acknowledged_no_target_retirement(acknowledgement)?;
        let mut native = self
            .native_acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            native.is_none(),
            "Stop already consumed its original native retirement ACK"
        );
        let mut projection = self
            .acknowledged
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = projection.as_ref().unwrap_or(&self.original);
        anyhow::ensure!(
            current.births.as_ref() == acknowledgement.original().births.as_ref()
                && current.same_creation_scope(
                    acknowledgement.original().create_coverage,
                    &acknowledgement.original().creations
                ),
            "no-target ACK replaced Stop's retained native source"
        );
        let output = std::sync::Arc::new(LaunchOrigin {
            plan: current.plan.clone(),
            generation: current.generation,
            births: acknowledgement.derived().births.clone(),
            creations: current.creations.clone(),
            create_coverage: current.create_coverage,
        });
        *projection = Some(output);
        *native = Some(acknowledgement);
        Ok(())
    }

    fn validate_acknowledged_native_history(&self, row: &Instance) -> Result<()> {
        self.native_origin().validate_native_history(row)
    }
}

impl Drop for OwnedStop {
    fn drop(&mut self) {
        if self.operation != LifecycleOperation::Stop
            || self.finished.load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let original = self.current_projection();
        let generation = self.generation;
        let result = std::thread::Builder::new().name("aoe-owned-stop-retirement".into()).spawn(move || {
            if let Err(error) = release_stop_claim(&original, generation) {
                tracing::debug!(target: "acp.supervisor", session = %original.session_id(), %error, "original stop retirement preserved a changed scope");
            }
        });
        if let Err(error) = result {
            tracing::warn!(target: "acp.supervisor", %error, "owned stop retirement could not start; reservation remains fenced");
        }
    }
}

/// Claim exactly the cached original row. The committed generation is never rolled back.
pub(crate) fn reserve_owned_stop(
    storage: &Storage,
    expected: &Instance,
    require_idle: bool,
) -> Result<std::sync::Arc<OwnedStop>> {
    let original = LaunchOrigin::capture(expected)?;
    anyhow::ensure!(
        storage.same_origin_as(original.storage()),
        "stop replaced its cached physical original"
    );
    reserve_stop_from_origin(original, require_idle)
}

pub(crate) fn reserve_stop_from_origin(
    original: std::sync::Arc<LaunchOrigin>,
    require_idle: bool,
) -> Result<std::sync::Arc<OwnedStop>> {
    let storage = original.storage();
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(original.session_id())?;
    ensure_unique_owner(storage, original.session_id())?;
    let (generation, acknowledged) = storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == original.session_id())
            .context("session disappeared before stop claim")?;
        original.validate_baseline_at(row, original.generation())?;
        anyhow::ensure!(
            !require_idle
                || (!row.status.blocks_worktree_edit()
                    && row.runner_journal.preparations.is_empty()),
            "stop the session before moving its checkout"
        );
        let generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .map_err(anyhow::Error::new)?;
        row.storage_origin = Some(original.plan.storage.clone());
        let mut acknowledged = LaunchOrigin::capture_baseline(row)?;
        acknowledged.births = original.births.clone();
        Ok((generation, std::sync::Arc::new(acknowledged)))
    })?;
    let stop = OwnedStop::new(original, generation, LifecycleOperation::Stop);
    *stop
        .acknowledged
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(acknowledged);
    Ok(stop)
}

fn release_stop_claim(original: &LaunchOrigin, generation: u64) -> Result<()> {
    let storage = original.storage();
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(original.session_id())?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == original.session_id())
        .context("session disappeared during stop retirement")?;
    if !row.lifecycle_reservation_is_owned(LifecycleOperation::Stop, generation) {
        return Ok(());
    }
    original.validate_baseline_at(&row, generation)?;
    storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == original.session_id())
            .context("session disappeared during stop retirement")?;
        original.validate_baseline_at(row, generation)?;
        anyhow::ensure!(
            row.release_lifecycle_reservation_if_owned(LifecycleOperation::Stop, generation),
            "stop reservation was superseded"
        );
        Ok(())
    })
}

pub(crate) fn release_owned_stop(stop: &OwnedStop) -> Result<()> {
    anyhow::ensure!(
        stop.operation == LifecycleOperation::Stop,
        "Purge claim belongs to its transaction driver"
    );
    release_stop_claim(&stop.current_projection(), stop.generation)?;
    stop.finished
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Revalidate the original owner and settled journal, then release our Stop
/// immediately before the effect while retaining workspace, identity and
/// lifecycle fences. Cross-profile moves acquire their own reservation.
fn release_settled_stop(row: &mut Instance, generation: u64) -> Result<()> {
    anyhow::ensure!(
        row.lifecycle_reservation_is_owned(LifecycleOperation::Stop, generation),
        "stop reservation was superseded before effect"
    );
    anyhow::ensure!(
        row.runner_journal.proves_quiescent(),
        "runner journal is not proven quiescent"
    );
    row.release_lifecycle_reservation_if_owned(LifecycleOperation::Stop, generation);
    Ok(())
}

/// Caller retains workspace, identity and this instance lifecycle fences.
pub(crate) fn release_settled_stop_under_locks(stop: &OwnedStop) -> Result<()> {
    let storage = stop.storage();
    let id = stop.session_id();
    anyhow::ensure!(
        stop.operation == LifecycleOperation::Stop,
        "Purge claim belongs to its transaction driver"
    );
    storage.verify_profile_identity()?;
    storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .context("session disappeared after stop")?;
        stop.current_projection()
            .validate_baseline_at(row, stop.generation)?;
        stop.validate_acknowledged_native_history(row)?;
        release_settled_stop(row, stop.generation)
    })?;
    stop.finished
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

pub(crate) fn finish_owned_stop<T>(
    stop: &OwnedStop,
    effect: impl FnOnce(&Instance) -> Result<T>,
) -> Result<T> {
    let storage = stop.storage();
    let id = stop.session_id();
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(storage, id)?;
    let row = storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .context("session disappeared after stop")?;
        stop.current_projection()
            .validate_baseline_at(row, stop.generation)?;
        stop.validate_acknowledged_native_history(row)?;
        release_settled_stop(row, stop.generation)?;
        Ok(row.clone())
    })?;
    stop.finished
        .store(true, std::sync::atomic::Ordering::Release);
    effect(&row)
}

pub(crate) struct ManagedLaunch {
    profile: String,
    session_id: String,
    nonce: Uuid,
    generation: u64,
}

impl ManagedLaunch {
    pub(crate) fn new(owner: SessionPathOwner<'_>, generation: u64) -> Result<Self> {
        anyhow::ensure!(
            !owner.profile.is_empty() && !owner.session_id.is_empty(),
            "detached runner requires an explicit stored owner"
        );
        Ok(Self {
            profile: owner.profile.to_owned(),
            session_id: owner.session_id.to_owned(),
            nonce: Uuid::new_v4(),
            generation,
        })
    }

    pub(crate) fn nonce(&self) -> Uuid {
        self.nonce
    }

    pub(crate) fn configure(&self, command: &mut std::process::Command) {
        command.arg("--managed-profile").arg(&self.profile);
        command.arg("--launch-nonce").arg(self.nonce.to_string());
    }

    pub(crate) fn spawn(
        self,
        storage: &Storage,
        command: &mut tokio::process::Command,
        admission: Option<&crate::acp::runner_lifecycle::ExecutionAdmission>,
        job: crate::acp::runner_lifecycle::ExecutionJob,
        capture: impl FnMut(crate::acp::runner_lifecycle::RunnerIdentity),
    ) -> Result<u32> {
        let (custody, pid) = self.spawn_child(
            storage,
            admission,
            job,
            |input| {
                command.stdin(input);
                let spawned = command.spawn();
                command.stdin(std::process::Stdio::null());
                let child = spawned?;
                Ok((child.id(), child))
            },
            |child| {
                tokio::runtime::Handle::current().block_on(async {
                    if child.try_wait()?.is_none() {
                        let killed = child.start_kill();
                        child.wait().await.with_context(|| {
                            format!("reaping original runner after kill {killed:?}")
                        })?;
                    }
                    Ok(())
                })
            },
            capture,
        )?;
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_millis(25));
            loop {
                poll.tick().await;
                match custody.observe_original_terminal() {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(target: "acp", "original root wait remains protected: {error:#}");
                        break;
                    }
                }
            }
        });
        pid.context("runner exited before identification")
    }

    #[cfg(test)]
    pub(crate) fn spawn_owned(
        self,
        storage: &Storage,
        command: &mut std::process::Command,
        admission: &crate::acp::runner_lifecycle::ExecutionAdmission,
        job: crate::acp::runner_lifecycle::ExecutionJob,
    ) -> Result<custody::ManagedChild> {
        let (custody, pid) = self.spawn_child(
            storage,
            Some(admission),
            job,
            |input| {
                command.stdin(input);
                let spawned = command.spawn();
                command.stdin(std::process::Stdio::null());
                let child = spawned?;
                Ok((Some(child.id()), child))
            },
            |child| {
                if child.try_wait()?.is_none() {
                    let killed = child.kill();
                    child.wait().with_context(|| {
                        format!("reaping original runner after kill {killed:?}")
                    })?;
                }
                Ok(())
            },
            |_| {},
        )?;
        Ok(custody::ManagedChild {
            custody,
            pid: pid.context("original child PID is absent")?,
        })
    }

    fn spawn_child<C: custody::OriginalChild>(
        self,
        storage: &Storage,
        admission: Option<&crate::acp::runner_lifecycle::ExecutionAdmission>,
        job: crate::acp::runner_lifecycle::ExecutionJob,
        spawn: impl FnOnce(std::process::Stdio) -> Result<(Option<u32>, C)>,
        abort_and_reap: impl FnOnce(&mut C) -> Result<()>,
        mut capture: impl FnMut(crate::acp::runner_lifecycle::RunnerIdentity),
    ) -> Result<(
        std::sync::Arc<custody::OriginalLaunchCustody<C>>,
        Option<u32>,
    )> {
        let admission = admission.context("managed launch has no native execution admission")?;
        let original = admission
            .origin()
            .context("managed launch has no original authority")?;
        let custody = custody::OriginalLaunchCustody::register(
            self.nonce,
            original.clone(),
            admission.clone(),
            job,
        )?;
        let boot = current_boot().context("verified boot identity is unavailable")?;
        let workspace_fence = super::acquire_session_workspace_claim_lock()?;
        let identity_fence = super::acquire_session_identity_lock()?;
        anyhow::ensure!(
            storage.same_origin_as(original.storage()) && self.generation == original.generation(),
            "managed launch replaced its original claimed profile or authority epoch"
        );
        storage.verify_profile_identity()?;
        anyhow::ensure!(
            storage.original_profile_identity()?.is_durable(),
            "fresh native launch has no durable original profile birth time"
        );
        let lifecycle_fence = storage.acquire_instance_lifecycle_lock(&self.session_id)?;
        ensure_unique_owner(storage, &self.session_id)?;
        custody.lock().fences = Some([workspace_fence, identity_fence, lifecycle_fence]);
        let nonce = *self.nonce.as_bytes();
        let mut pid = None;
        let published = (|| -> Result<()> {
            storage.update_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.session_id)
                    .context("managed runner's session no longer exists")?;
                original.validate_row(row)?;
                let owned_preparation = admission.preparation_nonce();
                anyhow::ensure!(
                    owned_preparation.is_some()
                        && row
                            .runner_journal
                            .preparations
                            .iter()
                            .any(|ticket| ticket.boot == boot
                                && ticket.generation == self.generation
                                && Some(ticket.nonce) == owned_preparation)
                        && row
                            .runner_journal
                            .preparations
                            .iter()
                            .all(|ticket| Some(ticket.nonce) == owned_preparation),
                    "managed launch lost its exact claimed preparation"
                );
                anyhow::ensure!(
                    matches!(row.runner_journal.coverage, Coverage::Complete)
                        && row.runner_journal.launches().is_empty(),
                    "runner history does not prove quiescence; fresh launch remains protected"
                );
                if let Some(record) =
                    crate::process::worker_registry::load_strict(&self.session_id)?
                {
                    retire_quiescent_record_locked(
                        storage,
                        &record,
                        &row.runner_journal,
                        owned_preparation,
                    )?;
                    anyhow::ensure!(
                        crate::process::worker_registry::load_strict(&self.session_id)?.is_none(),
                        "a different registry ticket still owns this session namespace"
                    );
                }
                row.runner_journal.launches_mut().push(RunnerLaunch {
                    nonce,
                    boot,
                    generation: self.generation,
                    incarnation: None,
                    profile_identity: Some(storage.original_profile_identity()?),
                    phase: NativeLaunchPhase::Natal,
                    stop_endpoint: None,
                    registry: None,
                });
                Ok(())
            })?;
            sync_parent_directory(storage.sessions_path())?;
            let pending = original.with_pending_birth(nonce, boot)?;
            admission.record_produced_origin(&original, pending.clone(), None)?;
            custody.lock().original = pending.clone();
            let (authorization, input) = std::os::unix::net::UnixStream::pair()?;
            let watchdog = bootstrap::ParentNatalGuard::start(
                &authorization,
                admission.clone(),
                std::time::Instant::now() + bootstrap::NATAL_LIFETIME,
            )?;
            {
                let mut state = custody.lock();
                state.channel = Some(authorization);
                state.watchdog = Some(watchdog);
                state.spawn_attempted = true;
            }
            let input: std::os::fd::OwnedFd = input.into();
            let (spawned_pid, child) = spawn(std::process::Stdio::from(input))?;
            pid = spawned_pid;
            custody.lock().child = Some(child);
            let pid = pid.context("runner exited before identification")?;
            let profile_identity = storage.original_profile_identity()?;
            let incarnation = crate::process::process_incarnation(pid)?
                .context("runner incarnation is unavailable")?;
            anyhow::ensure!(
                incarnation.group == pid,
                "runner does not lead its process group"
            );
            let identity = crate::acp::runner_lifecycle::RunnerIdentity {
                pid,
                generation: self.generation,
                launch_nonce: Some(self.nonce),
                incarnation: Some(incarnation),
                profile_identity: Some(profile_identity),
                boot: Some(boot),
            };
            {
                let mut state = custody.lock();
                state.identity = Some(identity);
                state.observation = Some((
                    identity,
                    crate::process::OriginalRootDeathObservation::bind(incarnation)?,
                ));
            }
            storage.update_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.session_id)
                    .context("managed runner's session disappeared")?;
                pending.validate_row(row)?;
                let mut slots = row
                    .runner_journal
                    .launches_mut()
                    .iter_mut()
                    .filter(|launch| {
                        launch.nonce == nonce
                            && launch.boot == boot
                            && launch.generation == self.generation
                            && launch.profile_identity == Some(profile_identity)
                    });
                let launch = slots
                    .next()
                    .context("runner authorization was superseded")?;
                anyhow::ensure!(
                    slots.next().is_none()
                        && launch.phase == NativeLaunchPhase::Natal
                        && launch.incarnation.is_none(),
                    "original Natal slot was rebound"
                );
                launch.incarnation = Some(incarnation);
                launch.phase = NativeLaunchPhase::Armed;
                Ok(())
            })?;
            sync_parent_directory(storage.sessions_path())?;
            let issued = pending.with_issued_birth(identity)?;
            admission.record_produced_origin(&pending, issued.clone(), Some(identity))?;
            custody.lock().original = issued.clone();
            capture(identity);
            let armed = {
                let mut state = custody.lock();
                let custody::LaunchState {
                    channel,
                    fences,
                    authorization_attempted,
                    original: custody_original,
                    ..
                } = &mut *state;
                let channel = channel
                    .as_mut()
                    .context("original authorization channel is absent")?;
                let fences = fences
                    .as_ref()
                    .context("original constructor fences are absent")?;
                let armed = bootstrap::publish_original(
                    channel,
                    &issued,
                    identity,
                    [&fences[0], &fences[1], &fences[2]],
                )?;
                admission.record_produced_origin(&issued, armed.clone(), Some(identity))?;
                *custody_original = armed.clone();
                admission.authorize(identity, || {
                    *authorization_attempted = true;
                    channel.write_all(&[1])
                })?;
                armed
            };
            storage.update_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.session_id)
                    .context("Published original session disappeared")?;
                armed.validate_row(row)?;
                let mut slots = row
                    .runner_journal
                    .launches_mut()
                    .iter_mut()
                    .filter(|launch| {
                        launch.birth_key()
                            == NativeBirthKey {
                                nonce,
                                boot,
                                generation: identity.generation,
                                incarnation: identity.incarnation,
                                profile_identity: identity.profile_identity,
                            }
                    });
                let launch = slots.next().context("original Armed birth disappeared")?;
                anyhow::ensure!(
                    slots.next().is_none()
                        && launch.phase == NativeLaunchPhase::Armed
                        && launch.stop_endpoint.is_some()
                        && launch.registry.is_some(),
                    "Published authorization lacks its original Armed publication"
                );
                launch.phase = NativeLaunchPhase::Published;
                Ok(())
            })?;
            sync_parent_directory(storage.sessions_path())?;
            let committed = {
                let mut state = custody.lock();
                let channel = state
                    .channel
                    .as_mut()
                    .context("original commit channel is absent")?;
                bootstrap::commit_original(channel, &armed, identity)?
            };
            admission.record_produced_origin(&armed, committed.clone(), Some(identity))?;
            {
                let mut state = custody.lock();
                state.original = committed;
                state
                    .watchdog
                    .as_mut()
                    .context("original natal watchdog is absent")?
                    .finish()?;
            }
            Ok(())
        })();
        if let Err(error) = published {
            let refusal = mark_original_refusal(&custody, nonce, boot);
            custody.close_parent_channels();
            let reaped = {
                let mut state = custody.lock();
                let attempted = state.spawn_attempted;
                match state.child.as_mut() {
                    Some(child) => abort_and_reap(child),
                    None if !attempted => Ok(()),
                    None => Err(anyhow::anyhow!(
                        "spawn attempted without returned original Child"
                    )),
                }
            };
            let terminal = custody.observe_original_terminal();
            let retired = refusal.and_then(|launch| {
                anyhow::ensure!(terminal?, "original root remains live");
                if custody.lock().authorization_attempted {
                    custody.release_failed_fences(&launch)?;
                    anyhow::bail!(
                        "possibly-authorized original domain lacks its retirement receipt"
                    );
                }
                let receipt = custody.no_target_receipt(launch)?;
                let removed = retire_original_no_target(&receipt);
                custody.release_failed_fences(receipt.launch())?;
                let ack = removed?;
                custody
                    .admission
                    .acknowledge_no_target_retirement(ack.clone())?;
                let retirement = custody
                    .admission
                    .preparation_retirement()
                    .context("original preparation ACK channel disappeared")?;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    match *retirement.borrow() {
                        Some(true) => break,
                        Some(false) => {
                            anyhow::bail!("original preparation writer refused durable retirement")
                        }
                        None => {}
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "original preparation durable ACK deadline expired"
                    );
                    std::thread::park_timeout(Duration::from_millis(25));
                }
                custody.release_acknowledged(&ack)
            });
            return match retired {
                Ok(()) => Err(error),
                Err(retirement) => Err(error.context(format!(
                    "original constructor remains protected: retirement={retirement:#}; wait={reaped:?}"
                ))),
            };
        }
        custody.close_parent_channels();
        drop(custody.lock().fences.take());
        Ok((custody, pid))
    }
}

fn record_stop_endpoint_under_original_fences(
    original: &LaunchOrigin,
    born: crate::acp::runner_lifecycle::RunnerIdentity,
    endpoint: SocketEndpointIdentity,
) -> Result<()> {
    let storage = original.storage();
    storage.verify_profile_identity()?;
    ensure_unique_owner(storage, original.session_id())?;
    anyhow::ensure!(
        born.birth_is_complete()
            && born.pid == std::process::id()
            && born.boot == current_boot()
            && born.incarnation == crate::process::process_incarnation(born.pid)?
            && born.profile_identity == Some(storage.original_profile_identity()?)
            && endpoint.is_durable(),
        "natal publication replaced its original native birth or endpoint"
    );
    storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == original.session_id())
            .context("runner's original session disappeared")?;
        original.validate_row(row)?;
        let launch = row
            .runner_journal
            .launches_mut()
            .iter_mut()
            .find(|launch| {
                Some(Uuid::from_bytes(launch.nonce)) == born.launch_nonce
                    && Some(launch.boot) == born.boot
                    && launch.generation == born.generation
                    && launch.incarnation == born.incarnation
                    && launch.profile_identity == born.profile_identity
            })
            .context("runner lacks its exact original native birth")?;
        anyhow::ensure!(
            launch.phase == NativeLaunchPhase::Armed
                && launch.stop_endpoint.is_none()
                && launch.registry.is_none(),
            "natal endpoint was already published"
        );
        launch.stop_endpoint = Some(endpoint);
        Ok(())
    })?;
    sync_parent_directory(storage.sessions_path())
}

// Bootstrap retains the issuer's actual physical fences, without reacquiring them.
fn record_unresolved_launch_under_original_fences(
    original: &LaunchOrigin,
    key: NativeBirthKey,
    may_authorize: bool,
    record: Option<&crate::process::worker_registry::WorkerRecord>,
) -> Result<RunnerLaunch> {
    let storage = original.storage();
    storage.verify_profile_identity()?;
    ensure_unique_owner(storage, original.session_id())?;
    let launch = storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == original.session_id())
            .context("unresolved original native owner disappeared")?;
        original.validate_row(row)?;
        let launch = row
            .runner_journal
            .launches_mut()
            .iter_mut()
            .find(|launch| {
                launch.birth_key() == key
                    || !may_authorize
                        && key.incarnation.is_some()
                        && launch.incarnation.is_none()
                        && launch.phase == NativeLaunchPhase::Natal
                        && launch.nonce == key.nonce
                        && launch.boot == key.boot
                        && launch.generation == key.generation
                        && launch.profile_identity == key.profile_identity
            })
            .context("unresolved transition lost its exact original native birth")?;
        if launch.incarnation.is_none() {
            launch.incarnation = key.incarnation;
        }
        let next = NativeLaunchPhase::Unresolved {
            may_authorize: may_authorize
                || matches!(
                    launch.phase,
                    NativeLaunchPhase::Published
                        | NativeLaunchPhase::Unresolved {
                            may_authorize: true
                        }
                ),
        };
        anyhow::ensure!(
            launch.phase.allows_transition_to(next),
            "unresolved transition would weaken original launch authority"
        );
        if let Some(record) = record {
            anyhow::ensure!(
                launch.matches_birth(record),
                "unresolved publication belongs to a replacement native birth"
            );
            let witness = RegistryWitness {
                record_file_identity: record
                    .record_file_identity
                    .context("unresolved publication lacks original writer FD")?,
                control_file_identity: record
                    .control_file_identity
                    .context("unresolved publication lacks original control birth")?,
                socket_path: record.socket_path.clone(),
            };
            anyhow::ensure!(
                witness.record_file_identity.is_durable()
                    && witness.control_file_identity.is_durable(),
                "unresolved publication has no durable original resource witnesses"
            );
            if let Some(before) = &launch.registry {
                anyhow::ensure!(
                    before.control_file_identity == witness.control_file_identity
                        && before.socket_path == witness.socket_path,
                    "unresolved transition replaced original control evidence"
                );
            }
            launch.registry = Some(witness);
        }
        launch.phase = next;
        Ok(launch.clone())
    })?;
    sync_parent_directory(storage.sessions_path())?;
    Ok(launch)
}

fn publish_registry_under_original_fences<T>(
    original: &LaunchOrigin,
    record: &mut crate::process::worker_registry::WorkerRecord,
    publish: impl FnOnce(&mut crate::process::worker_registry::WorkerRecord) -> Result<T>,
) -> Result<T> {
    let storage = original.storage();
    storage.verify_profile_identity()?;
    let profile_identity = storage.original_profile_identity()?;
    let boot = current_boot().context("verified boot identity is unavailable")?;
    anyhow::ensure!(
        record.pid == std::process::id()
            && record.boot == Some(boot)
            && record.profile_identity == Some(profile_identity)
            && record.source_profile.as_deref() == Some(original.profile()),
        "registry publication is not this original native runner"
    );
    original.validate_record_birth(record)?;
    ensure_unique_owner(storage, &record.session_id)?;
    let original_key = NativeBirthKey {
        nonce: *record
            .launch_nonce
            .context("original publication lacks its nonce")?
            .as_bytes(),
        boot: record.boot.context("original publication lacks its boot")?,
        generation: record.generation,
        incarnation: record.incarnation,
        profile_identity: record.profile_identity,
    };
    let result = storage
        .update_under_workspace_claim_lock(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == record.session_id)
                .context("native registry's original session disappeared")?;
            original.validate_row(row)?;
            let launch = row
                .runner_journal
                .launches_mut()
                .iter_mut()
                .find(|launch| launch.matches_birth(record))
                .context("registry lacks its exact original native birth ticket")?;
            anyhow::ensure!(
                launch.phase == NativeLaunchPhase::Armed
                    && launch.registry.is_none()
                    && launch
                        .stop_endpoint
                        .is_some_and(|endpoint| endpoint.is_durable()),
                "native registry witness was already published"
            );
            let published = publish(record)?;
            let record_file_identity = record
                .record_file_identity
                .context("published registry has no actual writer FD")?;
            let control_file_identity = record
                .control_file_identity
                .context("published control endpoint has no actual birth witness")?;
            anyhow::ensure!(
                record_file_identity.is_durable()
                    && control_file_identity.is_durable()
                    && launch.matches_birth(record),
                "native registry publication lost its birth or actual resources"
            );
            launch.registry = Some(RegistryWitness {
                record_file_identity,
                control_file_identity,
                socket_path: record.socket_path.clone(),
            });

            Ok(published)
        })
        .and_then(|published| {
            sync_parent_directory(storage.sessions_path())?;
            Ok(published)
        });
    match result {
        Ok(published) => Ok(published),
        Err(error) => {
            let unchanged_birth = record.launch_nonce.map(|nonce| *nonce.as_bytes())
                == Some(original_key.nonce)
                && record.boot == Some(original_key.boot)
                && record.generation == original_key.generation
                && record.incarnation == original_key.incarnation
                && record.profile_identity == original_key.profile_identity
                && record
                    .incarnation
                    .is_some_and(|incarnation| incarnation.pid == record.pid);
            let evidence = record
                .record_file_identity
                .zip(record.control_file_identity)
                .filter(|(writer, control)| {
                    unchanged_birth && writer.is_durable() && control.is_durable()
                })
                .map(|_| &*record);
            match record_unresolved_launch_under_original_fences(original, original_key, true, evidence) {
                Ok(_) => Err(error),
                Err(retention_error) => Err(error.context(format!(
                    "original publication uncertainty could not be durably retained; keep original custody and fences: {retention_error:#}"
                ))),
            }
        }
    }
}

pub(crate) fn update_owned_registry_record(
    original: &LaunchOrigin,
    record: &mut crate::process::worker_registry::WorkerRecord,
    effect: impl FnOnce(&mut crate::process::worker_registry::WorkerRecord) -> Result<()>,
) -> Result<()> {
    let storage = original.storage();
    original.validate_record_birth(record)?;
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(&record.session_id)?;
    ensure_unique_owner(storage, &record.session_id)?;
    let profile_identity = storage.original_profile_identity()?;
    let boot = current_boot().context("verified boot identity is unavailable")?;
    anyhow::ensure!(
        record.pid == std::process::id()
            && record.profile_identity == Some(profile_identity)
            && record.boot == Some(boot),
        "self update is not this original native runner"
    );
    storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == record.session_id)
            .context("self-update original session disappeared")?;
        original.validate_row(row)?;
        let launch = row
            .runner_journal
            .launches_mut()
            .iter_mut()
            .find(|launch| launch.matches_birth(record))
            .context("self update lacks its exact immutable native birth ticket")?;
        anyhow::ensure!(
            launch.is_published(),
            "self update cannot publish or heal a pending/uncertain original"
        );
        let witness = launch
            .registry
            .as_ref()
            .context("self update lacks its published registry witness")?;
        anyhow::ensure!(
            witness.matches_record(record),
            "owned native registry witness changed before self update"
        );
        effect(record)?;
        let actual = record
            .record_file_identity
            .context("self writer returned no actual FD witness")?;
        anyhow::ensure!(
            actual.is_durable()
                && launch.matches_birth(record)
                && record.control_file_identity == Some(witness.control_file_identity)
                && record.socket_path == witness.socket_path,
            "self update changed immutable native birth or control custody"
        );
        launch
            .registry
            .as_mut()
            .expect("validated native witness")
            .record_file_identity = actual;
        Ok(())
    })?;
    sync_parent_directory(storage.sessions_path())
}

/// Admit a resident runner prompt under checkout-move physical fences.
pub(crate) fn admit_runner_prompt<T>(
    storage: &Storage,
    id: &str,
    pid: u32,
    generation: u64,
    nonce: Uuid,
    admit: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(storage, id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("runner session disappeared before prompt admission")?;
    startable(&row)?;
    anyhow::ensure!(
        !row.lifecycle_reservation
            .as_ref()
            .is_some_and(|claim| claim.op == LifecycleOperation::Stop),
        "session checkout is reserved for a move"
    );
    let record = crate::process::worker_registry::load_strict(id)?
        .context("runner prompt lost its actual Published registry witness")?;
    anyhow::ensure!(
        row.runner_journal.owns_record(&record),
        "runner prompt is not owned by its explicitly Published original"
    );
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let incarnation =
        crate::process::process_incarnation(pid)?.context("runner incarnation is unavailable")?;
    let profile_identity = storage.original_profile_identity()?;
    anyhow::ensure!(
        row.runner_journal.launches().iter().any(|launch| {
            launch.is_published()
                && launch.matches_birth(&record)
                && launch.nonce == *nonce.as_bytes()
                && launch.boot == boot
                && launch.generation == generation
                && launch.incarnation == Some(incarnation)
                && launch.profile_identity == Some(profile_identity)
        }),
        "runner prompt lacks its original execution authorization"
    );
    admit()
}

pub(crate) fn stop_socket(id: &str, pid: u32) -> Result<PathBuf> {
    let record = crate::process::worker_registry::record_path(id)?;
    Ok(record.with_file_name(format!("{id}.{pid}.stop")))
}

pub(crate) struct AcceptedStop {
    connection: tokio::net::UnixStream,
    nonce: Uuid,
    mode: u8,
}

impl AcceptedStop {
    pub(crate) fn forced(&self) -> bool {
        self.mode == 1
    }

    pub(crate) fn can_upgrade(&self) -> bool {
        self.mode == 0
    }

    pub(crate) async fn accept_force_upgrade(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.can_upgrade(),
            "only a graceful Stop accepts Force on its original stream"
        );
        let mut frame = [0_u8; 17];
        self.connection.read_exact(&mut frame).await?;
        anyhow::ensure!(
            frame[..16] == self.nonce.as_bytes()[..] && frame[16] == 1,
            "invalid original-stream Stop upgrade"
        );
        frame[16] = 1;
        tokio::time::timeout(Duration::from_secs(1), self.connection.write_all(&frame))
            .await
            .context("original-stream Force acknowledgement timed out")??;
        self.mode = 1;
        Ok(())
    }
    #[cfg(debug_assertions)]
    pub(crate) fn isolated_retirement_gate(
        &self,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let gate = (self.can_upgrade() && std::env::var_os("AOE_ISOLATED_RUNNER_TEST").is_some())
            .then(|| std::env::var_os("AOE_TEST_STOP_RETIRE_GATE"))
            .flatten()
            .map(PathBuf::from);
        let nonce = self.nonce;
        async move {
            if let Some(gate) = gate {
                tokio::fs::write(
                    gate.with_extension("entered"),
                    serde_json::to_vec(&serde_json::json!({
                        "pid": std::process::id(), "nonce": nonce, "mode": 0
                    }))?,
                )
                .await?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                while !tokio::fs::try_exists(&gate).await? {
                    anyhow::ensure!(
                        tokio::time::Instant::now() < deadline,
                        "isolated original retirement gate was not released"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            Ok(())
        }
    }
}

pub(crate) async fn wait_for_stop<F: std::future::Future<Output = bool>>(
    listener: std::sync::Arc<tokio::net::UnixListener>,
    nonce: Uuid,
    admit: impl Fn(bool) -> F,
) -> Result<AcceptedStop> {
    loop {
        let (mut connection, _) = listener.accept().await?;
        let mut received = [0; 17];
        if matches!(
            tokio::time::timeout(Duration::from_secs(2), connection.read_exact(&mut received))
                .await,
            Ok(Ok(_))
        ) && received[..16] == nonce.as_bytes()[..]
            && received[16] <= 2
        {
            let accepted = admit(received[16] == 2).await;
            let mut receipt = [0; 17];
            receipt[..16].copy_from_slice(nonce.as_bytes());
            receipt[16] = u8::from(accepted);
            let acknowledged =
                tokio::time::timeout(Duration::from_secs(1), connection.write_all(&receipt)).await;
            if accepted {
                acknowledged.context("original Stop acknowledgement timed out")??;
                return Ok(AcceptedStop {
                    connection,
                    nonce,
                    mode: received[16],
                });
            }
        }
    }
}
/// Initial standalone-command lookup: keep the same inventory fences through
/// the original row and physical profile capture, before any async operation.
pub(crate) fn capture_unique_origin(id: &str) -> Result<std::sync::Arc<LaunchOrigin>> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let storage =
        find_stored_owner_locked(id)?.context("runner has no authoritative stored owner")?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    let mut row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("original runner owner disappeared during capture")?;
    row.storage_origin = Some(std::sync::Arc::new(storage));
    LaunchOrigin::capture(&row)
}
fn find_stored_owner_locked(id: &str) -> Result<Option<Storage>> {
    super::retained_intents::ensure_id_available(id)?;
    let mut found: Option<(Storage, std::fs::Metadata)> = None;
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
            None => found = Some((storage, metadata)),
        }
    }
    Ok(found.map(|(storage, _)| storage))
}

pub(crate) async fn settle_captured_ticket(
    id: &str,
    identity: crate::acp::runner_lifecycle::RunnerIdentity,
    _force: bool,
) -> Result<()> {
    anyhow::ensure!(
        identity.birth_is_complete(),
        "captured runner lacks its complete original native birth"
    );
    anyhow::bail!(
        "captured runner {id:?} supplies identity, not an authenticated original full-domain/no-target receipt and producer durable ACK; retain its original custody and protected admission"
    )
}

fn retire_quiescent_record_locked(
    storage: &Storage,
    record: &crate::process::worker_registry::WorkerRecord,
    journal: &RunnerExecutionJournal,
    _owned_preparation: Option<[u8; 16]>,
) -> Result<()> {
    storage.verify_profile_identity()?;
    let identity = crate::acp::runner_lifecycle::RunnerIdentity {
        pid: record.pid,
        generation: record.generation,
        launch_nonce: record.launch_nonce,
        incarnation: record.incarnation,
        profile_identity: record.profile_identity,
        boot: record.boot,
    };
    anyhow::ensure!(
        identity.birth_is_complete(),
        "registry resource lacks its original native birth"
    );
    anyhow::ensure!(
        identity.profile_identity == Some(storage.original_profile_identity()?),
        "registry execution was born in another physical profile"
    );
    anyhow::ensure!(
        journal.owns_record(record),
        "registry resource lacks its actual original Published witness"
    );
    anyhow::bail!(
        "original registry retirement requires an authenticated full-domain/no-target receipt and producer durable ACK; registry absence, old boot and root death do not authorize retirement"
    )
}
#[derive(Clone)]
pub(crate) enum JournalScope {
    Stop(std::sync::Arc<OwnedStop>),
    Launch(std::sync::Arc<LaunchOrigin>),
}
impl JournalScope {
    fn proves_settled(&self, journal: &RunnerExecutionJournal, nonce: Option<[u8; 16]>) -> bool {
        let destructive =
            matches!(self, Self::Stop(stop) if stop.operation == LifecycleOperation::Purge);
        if destructive {
            journal.proves_quiescent() && journal.proves_for(nonce)
        } else {
            journal.proves_for(nonce)
        }
    }

    fn storage(&self) -> &Storage {
        match self {
            Self::Stop(stop) => stop.storage(),
            Self::Launch(origin) => origin.storage(),
        }
    }
    fn session_id(&self) -> &str {
        match self {
            Self::Stop(stop) => stop.session_id(),
            Self::Launch(origin) => origin.session_id(),
        }
    }
    fn validate(&self, row: &Instance) -> Result<()> {
        match self {
            Self::Stop(stop) => {
                stop.current_projection()
                    .validate_baseline_at(row, stop.generation)?;
                stop.validate_acknowledged_native_history(row)?;
                anyhow::ensure!(
                    row.lifecycle_reservation_is_owned(stop.operation, stop.generation),
                    "original lifecycle receipt was superseded before runner settlement"
                );
                Ok(())
            }
            Self::Launch(origin) => origin.validate_baseline_at(row, origin.generation()),
        }
    }
}

fn snapshot(scope: &JournalScope, _nonce: Option<[u8; 16]>) -> Result<RunnerExecutionJournal> {
    let storage = scope.storage();
    let id = scope.session_id();
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(storage, id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("runner's stored owner disappeared without original retirement acknowledgement")?;
    scope.validate(&row)?;
    validate_launch_snapshots(row.runner_journal.launches())?;
    Ok(row.runner_journal)
}

pub(crate) async fn settle(stop: std::sync::Arc<OwnedStop>) -> Result<()> {
    settle_selected(JournalScope::Stop(stop), None, 0).await
}

pub(crate) async fn kill(stop: std::sync::Arc<OwnedStop>) -> Result<()> {
    settle_selected(JournalScope::Stop(stop), None, 1).await
}

pub(crate) async fn settle_if_idle(stop: std::sync::Arc<OwnedStop>) -> Result<()> {
    settle_selected(JournalScope::Stop(stop), None, 2).await
}

#[cfg(test)]
pub(crate) async fn require_quiescent(scope: JournalScope) -> Result<()> {
    let journal = tokio::task::spawn_blocking(move || snapshot(&scope, None)).await??;
    anyhow::ensure!(
        journal.proves_quiescent(),
        "runner execution is not proven quiescent; no runner was signalled"
    );
    Ok(())
}

fn for_each_stored_session(
    mut visit: impl FnMut(Instance),
) -> Result<std::collections::HashSet<String>> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let retained = super::retained_intents::retained_ids_in(&super::get_app_dir()?)?;
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
            // Retained owners are exclusions only, including an interrupted abort
            // whose original profile row has not yet been removed.
            if !retained.contains(&row.id) {
                visit(row);
            }
        }
    }
    Ok(retained)
}

pub(crate) fn stored_session_ids() -> Result<Vec<String>> {
    let mut ids = std::collections::HashSet::new();
    let retained = for_each_stored_session(|row| {
        ids.insert(row.id);
    })?;
    ids.extend(retained);
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_unstable();
    Ok(ids)
}

pub(crate) fn retained_runner_session_ids() -> Result<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::new();
    let retained = for_each_stored_session(|row| {
        if row.runner_journal.has_unknown_runner_coverage()
            || !row.runner_journal.preparations.is_empty()
            || !row.runner_journal.launches().is_empty()
        {
            ids.insert(row.id);
        }
    })?;
    ids.extend(retained);
    Ok(ids)
}

pub(crate) async fn settle_nonce(scope: JournalScope, nonce: Uuid) -> Result<()> {
    settle_selected(scope, Some(*nonce.as_bytes()), 0).await
}

pub(crate) fn verify_published_runner(
    storage: &Storage,
    id: &str,
    nonce: Uuid,
    pid: u32,
    generation: u64,
) -> Result<crate::acp::runner_lifecycle::RunnerIdentity> {
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    storage.verify_profile_identity()?;
    let profile_identity = storage.original_profile_identity()?;
    let record =
        crate::process::worker_registry::load_strict(id)?.context("runner record disappeared")?;
    anyhow::ensure!(
        record.pid == pid
            && record.generation == generation
            && record.launch_nonce == Some(nonce)
            && record.boot == Some(boot)
            && record.profile_identity == Some(profile_identity),
        "runner registry lacks matching original birth and physical profile proof"
    );
    let incarnation = record
        .incarnation
        .context("runner registry lacks original native birth evidence")?;
    anyhow::ensure!(
        incarnation.pid == pid
            && incarnation.group == pid
            && crate::process::process_incarnation(pid)? == Some(incarnation)
            && crate::process::worker::is_process_group_alive(pid),
        "runner original group is not live"
    );
    let profile = record
        .source_profile
        .as_deref()
        .filter(|profile| !profile.is_empty())
        .context("runner record has no explicit stored owner")?;
    let other = Storage::open_unwatched(profile)?;
    anyhow::ensure!(
        storage.same_origin_as(&other),
        "runner record belongs to another physical profile"
    );
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    ensure_unique_owner(storage, id)?;
    let row = storage
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == id)
        .context("runner stored owner disappeared")?;
    anyhow::ensure!(
        row.runner_journal
            .launches()
            .iter()
            .any(|launch| launch.is_published()
                && launch.nonce == *nonce.as_bytes()
                && launch.boot == boot
                && launch.generation == generation
                && launch.incarnation == Some(incarnation)
                && launch.profile_identity == Some(profile_identity)
                && launch
                    .registry
                    .as_ref()
                    .is_some_and(|witness| witness.matches_record(&record))),
        "runner published execution does not match its original birth"
    );
    Ok(crate::acp::runner_lifecycle::RunnerIdentity {
        pid,
        generation,
        launch_nonce: Some(nonce),
        incarnation: Some(incarnation),
        profile_identity: Some(profile_identity),
        boot: Some(boot),
    })
}

fn validate_selected_births(storage: &Storage, id: &str, live: &[RunnerLaunch]) -> Result<()> {
    let boot = current_boot().context("verified boot identity is unavailable")?;
    let profile_identity = storage.original_profile_identity()?;
    let record = if live.iter().any(|launch| launch.stop_endpoint.is_none()) {
        crate::process::worker_registry::load_strict(id)?
    } else {
        None
    };
    validate_launch_snapshots(live)?;
    for launch in live {
        anyhow::ensure!(launch.is_published(),
            "pending or uncertain original native launch requires its custody-owned no-target/domain receipt; it is not a rediscovered Published Stop target");
        let incarnation = launch
            .incarnation
            .context("live runner has no native incarnation birth evidence")?;
        anyhow::ensure!(
            profile_identity.is_durable()
                && launch.boot == boot
                && launch.profile_identity == Some(profile_identity),
            "live runner belongs to another or unproven physical profile"
        );
        let identity = crate::acp::runner_lifecycle::RunnerIdentity {
            pid: incarnation.pid,
            generation: launch.generation,
            launch_nonce: Some(Uuid::from_bytes(launch.nonce)),
            incarnation: Some(incarnation),
            profile_identity: launch.profile_identity,
            boot: Some(launch.boot),
        };
        anyhow::ensure!(
            launch.stop_endpoint.is_some()
                || record
                    .as_ref()
                    .is_some_and(|record| identity.matches_record(record)),
            "live runner has no natal receipt or matching born publication"
        );
        anyhow::ensure!(
            launch
                .stop_endpoint
                .is_some_and(|endpoint| endpoint.is_durable()),
            "live runner has no durable natal stop endpoint identity"
        );
    }
    Ok(())
}

pub(crate) async fn settle_purge(
    stop: std::sync::Arc<OwnedStop>,
    control: crate::session::deletion::PurgeControl,
) -> Result<()> {
    anyhow::ensure!(
        control.owns_stop(&stop),
        "force control does not own this original purge receipt"
    );
    let driver = tokio::spawn(settle_selected_owned(
        JournalScope::Stop(stop),
        None,
        0,
        Some(control),
    ));
    driver.await.context("owned original purge force driver")?
}
async fn settle_selected(scope: JournalScope, nonce: Option<[u8; 16]>, mode: u8) -> Result<()> {
    let driver = tokio::spawn(settle_selected_owned(scope, nonce, mode, None));
    driver
        .await
        .context("owned original runner settlement driver")?
}

async fn settle_selected_owned(
    scope: JournalScope,
    nonce: Option<[u8; 16]>,
    mode: u8,
    control: Option<crate::session::deletion::PurgeControl>,
) -> Result<()> {
    let scope_read = scope.clone();
    let (live, quiescent) = tokio::task::spawn_blocking(move || {
        let mut journal = snapshot(&scope_read, nonce)?;
        let quiescent = scope_read.proves_settled(&journal, nonce);
        journal
            .launches
            .retain(|launch| nonce.is_none_or(|nonce| nonce == launch.nonce));
        validate_selected_births(
            scope_read.storage(),
            scope_read.session_id(),
            &journal.launches,
        )?;
        anyhow::Ok((journal.launches, quiescent))
    })
    .await
    .context("original runner birth proof job")??;
    anyhow::ensure!(
        quiescent || !live.is_empty(),
        "unknown coverage or unfinished original preparation lacks its explicit retirement ACK"
    );
    let mut authenticated_endpoints = Vec::new();
    let mut force_deadline = None;
    if !quiescent {
        for launch in &live {
            let incarnation = launch
                .incarnation
                .context("verified live birth lost its incarnation")?;
            let endpoint = launch
                .stop_endpoint
                .context("verified live birth lost its natal endpoint")?;
            let current_scope = scope.clone();
            let captured = tokio::task::spawn_blocking(move || {
                let path = stop_socket(current_scope.session_id(), incarnation.pid)?;
                snapshot(&current_scope, nonce)?;
                let identity = SocketEndpointIdentity::observe(&path)?;
                anyhow::ensure!(
                    identity.is_durable() && identity == endpoint,
                    "runner stop endpoint differs from its original natal socket"
                );
                anyhow::ensure!(
                    crate::process::process_incarnation(incarnation.pid)? == Some(incarnation),
                    "runner incarnation changed before stop"
                );
                Ok::<_, anyhow::Error>((path, identity))
            })
            .await?;
            let Ok((path, identity)) = captured else {
                continue;
            };
            let mut frame = [0; 17];
            frame[..16].copy_from_slice(&launch.nonce);
            let requested = async {
                let mut socket = tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::UnixStream::connect(&path),
                )
                .await
                .map_err(std::io::Error::other)??;
                if crate::process::worker::peer_pid_from_connected_socket(&socket)
                    != Some(incarnation.pid)
                {
                    return Err(std::io::Error::other(
                        "connected stop endpoint has a different kernel peer",
                    ));
                }
                let checked_path = path.clone();
                let connected_scope = scope.clone();
                let original_endpoint = tokio::task::spawn_blocking(move || {
                    snapshot(&connected_scope, nonce)?;
                    let metadata = std::fs::symlink_metadata(checked_path)?;
                    anyhow::ensure!(
                        endpoint.matches_metadata(&metadata)
                            && crate::process::process_incarnation(incarnation.pid)?
                                == Some(incarnation),
                        "connected stop endpoint lost its natal identity or incarnation"
                    );
                    anyhow::Ok(())
                })
                .await
                .map_err(std::io::Error::other)?;
                original_endpoint.map_err(std::io::Error::other)?;
                frame[16] = if control
                    .as_ref()
                    .is_some_and(|control| control.force_requested())
                {
                    1
                } else {
                    mode
                };
                if frame[16] == 1 {
                    if let (Some(control), JournalScope::Stop(stop)) = (&control, &scope) {
                        if !control.owns_stop(stop) {
                            return Err(std::io::Error::other(
                                "original purge owner was lost before forced Stop",
                            ));
                        }
                    }
                    force_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + Duration::from_secs(3)
                    });
                }
                let accepted = tokio::time::timeout(Duration::from_secs(1), async {
                    socket.write_all(&frame).await?;
                    let mut receipt = [0; 17];
                    socket.read_exact(&mut receipt).await?;
                    if receipt[..16] != launch.nonce {
                        return Err(std::io::Error::other(
                            "runner execution ticket was not authenticated",
                        ));
                    }
                    Ok::<_, std::io::Error>(receipt[16] == 1)
                })
                .await
                .map_err(std::io::Error::other)??;
                Ok::<_, std::io::Error>((accepted, socket))
            }
            .await;
            let accepted = match requested {
                Ok((true, connection)) => {
                    authenticated_endpoints.push((
                        path,
                        identity,
                        connection,
                        incarnation,
                        launch.nonce,
                        frame[16] == 1,
                    ));
                    true
                }
                Ok((false, _)) => {
                    anyhow::bail!("stop the session before moving its checkout; runner is busy")
                }
                _ => false,
            };
            anyhow::ensure!(
                mode != 2 || accepted,
                "cannot prove the original runner idle before moving its checkout"
            );
            if !accepted {
                tracing::debug!(target: "acp.supervisor", session = %scope.session_id(), pid = incarnation.pid, "runner stop endpoint unavailable");
            }
        }
        let graceful_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut upgraded = false;
        loop {
            let current_scope = scope.clone();
            let mut proof = tokio::task::spawn_blocking(move || snapshot(&current_scope, nonce));
            let journal = if let Some(control) = &control {
                loop {
                    tokio::select! {
                        result = &mut proof => break result??,
                        _ = control.changed(), if !upgraded => {}
                    }
                }
            } else {
                proof.await??
            };
            let current = scope.proves_settled(&journal, nonce);
            if !current
                && !upgraded
                && control
                    .as_ref()
                    .is_some_and(|control| control.force_requested())
            {
                let control = control
                    .as_ref()
                    .expect("force intent has an original control");
                let JournalScope::Stop(stop) = &scope else {
                    anyhow::bail!("force intent has no original purge receipt");
                };
                anyhow::ensure!(
                    control.owns_stop(stop),
                    "original purge owner was lost before force escalation"
                );
                for (_, _, connection, incarnation, ticket, forced) in &mut authenticated_endpoints
                {
                    if *forced {
                        continue;
                    }
                    let check_scope = scope.clone();
                    let expected = *incarnation;
                    let still_live = tokio::task::spawn_blocking(move || {
                        snapshot(&check_scope, nonce)?;
                        Ok::<_, anyhow::Error>(
                            crate::process::process_incarnation(expected.pid)? == Some(expected),
                        )
                    })
                    .await??;
                    if !still_live {
                        continue;
                    }
                    anyhow::ensure!(
                        control.owns_stop(stop),
                        "original purge owner was lost before force escalation"
                    );
                    anyhow::ensure!(
                        crate::process::worker::peer_pid_from_connected_socket(connection)
                            == Some(incarnation.pid),
                        "retained stop connection lost its original kernel peer"
                    );
                    let mut frame = [0; 17];
                    frame[..16].copy_from_slice(ticket);
                    frame[16] = 1;
                    force_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + Duration::from_secs(3)
                    });
                    tokio::time::timeout(Duration::from_secs(1), async {
                        connection.write_all(&frame).await?;
                        let mut receipt = [0; 17];
                        connection.read_exact(&mut receipt).await?;
                        anyhow::ensure!(
                            receipt[..16] == *ticket && receipt[16] == 1,
                            "original runner refused force escalation"
                        );
                        anyhow::Ok(())
                    })
                    .await
                    .context("original force receipt deadline")??;
                    *forced = true;
                }
                upgraded = true;
            }
            if current || tokio::time::Instant::now() >= force_deadline.unwrap_or(graceful_deadline)
            {
                break;
            }
            if let Some(control) = &control {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                    _ = control.changed(), if !upgraded => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    let quiescent = tokio::task::spawn_blocking(move || {
        let journal = snapshot(&scope, nonce)?;
        anyhow::ensure!(scope.proves_settled(&journal, nonce), "original native execution lacks an authenticated full-domain/no-target receipt and producer durable ACK; retain the session, checkout and original custody. Boot/root/group death and registry absence do not retire it");
        drop(authenticated_endpoints);
        anyhow::Ok(())
    }).await.context("original all-history settlement proof job")?;
    quiescent?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn published_cache_rejects_changed_create_coverage() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::create_profile("create-cache")?;
        let storage = Storage::new_unwatched("create-cache")?;
        let mut row = Instance::new("original", "/metadata-only");
        row.source_profile = "create-cache".into();
        row.runner_journal = RunnerExecutionJournal::legacy_unknown();
        storage.update(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        let mut cached = storage.load()?.remove(0);
        let original = LaunchOrigin::capture_baseline(&cached)?;
        assert!(original.recognizes_published_instance(&cached));
        let snapshot = LaunchOrigin::capture_baseline(&cached)?;
        assert!(original.recognizes_published_snapshot(&snapshot));
        cached.runner_journal.create_coverage = CreationCoverage::Owned;
        assert!(!original.recognizes_published_instance(&cached));
        let changed = LaunchOrigin::capture_baseline(&cached)?;
        assert!(!original.recognizes_published_snapshot(&changed));
        Ok(())
    }

    #[test]
    fn journal_rejects_missing_native_evidence_instead_of_assuming_quiescence() {
        let current = serde_json::to_value(RunnerExecutionJournal::new()).unwrap();
        for field in ["launches", "preparations", "creations", "create_coverage"] {
            for omitted in [true, false] {
                let mut partial = current.clone();
                if omitted {
                    partial.as_object_mut().unwrap().remove(field);
                } else {
                    partial[field] = serde_json::Value::Null;
                }
                assert!(
                    serde_json::from_value::<RunnerExecutionJournal>(partial).is_err(),
                    "{field}, omitted={omitted}"
                );
            }
        }
        let nonce = [1_u8; 16];
        let boot = [2_u8; 16];
        let mut pending = current.clone();
        pending["launches"] = serde_json::json!([{
            "nonce": nonce, "boot": boot, "generation": 0, "incarnation": null,
            "profile_identity": null,
            "phase": {"state": "natal"},
            "stop_endpoint": null, "registry": null,
        }]);
        let explicit: RunnerExecutionJournal = serde_json::from_value(pending.clone()).unwrap();
        assert!(!explicit.proves_runner_quiescent());
        assert!(!explicit.proves_for(Some(nonce)));
        assert!(!explicit.proves_quiescent());
        for state in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!({"state": "missing"})),
            Some(serde_json::json!({"state": "unresolved"})),
            Some(serde_json::json!({"state": "unresolved", "may_authorize": null})),
        ] {
            let mut partial = pending.clone();
            match state {
                Some(state) => partial["launches"][0]["phase"] = state,
                None => {
                    partial["launches"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("phase");
                }
            }
            assert!(serde_json::from_value::<RunnerExecutionJournal>(partial).is_err());
        }
        let mut missing_birth = pending.clone();
        missing_birth["launches"][0]
            .as_object_mut()
            .unwrap()
            .remove("incarnation");
        assert!(serde_json::from_value::<RunnerExecutionJournal>(missing_birth).is_err());
        for phase in [
            NativeLaunchPhase::Natal,
            NativeLaunchPhase::Armed,
            NativeLaunchPhase::Published,
            NativeLaunchPhase::Unresolved {
                may_authorize: false,
            },
            NativeLaunchPhase::Unresolved {
                may_authorize: true,
            },
        ] {
            for incarnation in [
                serde_json::Value::Null,
                serde_json::json!({
                    "pid": 6, "group": 6, "start": [7, 8], "namespace": [9, 10],
                }),
            ] {
                let mut protected = pending.clone();
                protected["launches"][0]["phase"] = serde_json::to_value(phase).unwrap();
                protected["launches"][0]["incarnation"] = incarnation;
                let protected: RunnerExecutionJournal = serde_json::from_value(protected).unwrap();
                assert!(!protected.proves_runner_quiescent(), "{phase:?}");
                assert!(!protected.proves_for(Some(nonce)), "{phase:?}");
            }
        }
        let mut old_unknown = current.clone();
        old_unknown["coverage"] = serde_json::json!("unknown");
        let old_boot = [3_u8; 16];
        old_unknown["boot"] = serde_json::json!(old_boot);
        let old_unknown: RunnerExecutionJournal = serde_json::from_value(old_unknown).unwrap();
        assert!(!old_unknown.proves_runner_quiescent());
        let mut preparing = current;
        preparing["preparations"] =
            serde_json::json!([{"nonce": nonce, "boot": boot, "generation": 0}]);
        let preparing: RunnerExecutionJournal = serde_json::from_value(preparing).unwrap();
        assert!(!preparing.proves_runner_quiescent());
    }

    #[test]
    #[serial_test::serial]
    fn retained_intent_refuses_original_scope_even_with_its_source_row() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = super::super::test_support::isolate_app_dir_at(temporary.path());
        super::super::retained_intents::initialize_legacy_in(&super::super::get_app_dir().unwrap())
            .unwrap();
        let storage = Storage::new_unwatched("default").unwrap();
        let mut row = Instance::new("retained", temporary.path().to_str().unwrap());
        row.runner_journal = RunnerExecutionJournal::legacy_unknown();
        row.status = super::super::Status::Creating;
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )
        .unwrap();
        row.lifecycle_reservation.as_mut().unwrap().path_claims =
            super::super::WorktreePathClaims::Unknown(None);
        let source = serde_json::to_vec(&vec![&row]).unwrap();
        std::fs::write(storage.sessions_path(), &source).unwrap();
        row.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        let original = LaunchOrigin::capture(&row).unwrap();
        original
            .validate_baseline_at(&row, row.lifecycle_generation)
            .unwrap();
        let selection = super::super::retained_intents::capture(&storage, &row.id).unwrap();
        super::super::retained_intents::abort(&selection).unwrap();
        // Recreate precisely the interrupted append-before-removal state.
        std::fs::write(storage.sessions_path(), &source).unwrap();
        assert!(LaunchOrigin::capture(&row).is_err());
        assert!(original
            .validate_baseline_at(&row, row.lifecycle_generation)
            .is_err());
        assert!(capture_unique_origin(&row.id).is_err());
        assert!(original
            .with_storage::<()>(|_, _| panic!("retained original ran an effect"))
            .is_err());
        assert!(admit_runner_prompt::<()>(
            &storage,
            &row.id,
            std::process::id(),
            row.lifecycle_generation,
            Uuid::nil(),
            || panic!("retained prompt ran an effect"),
        )
        .is_err());
        let exclusions = for_each_stored_session(|_| panic!("retained row was visited")).unwrap();
        assert!(exclusions.contains(&row.id));
        assert_eq!(stored_session_ids().unwrap(), vec![row.id]);
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), source);
    }

    #[test]
    #[serial_test::serial]
    fn merge_launch_plan_rejects_changed_naming_and_provider() {
        let temporary = tempfile::TempDir::new_in("/tmp").unwrap();
        let _app_dir = super::super::test_support::isolate_app_dir_at(temporary.path());
        let storage = std::sync::Arc::new(Storage::new_unwatched("default").unwrap());
        let mut row = Instance::new("typed", temporary.path().to_str().unwrap());
        row.first_launch_names_agent = true;
        row.agent_provider = Some("api".into());
        let original = LaunchOrigin::capture_baseline_at(&row, storage).unwrap();
        for naming_changed in [true, false] {
            let mut changed = row.clone();
            if naming_changed {
                changed.first_launch_names_agent = false;
            } else {
                changed.agent_provider = Some("vertex".into());
            }
            assert!(original
                .validate_plan_at(&changed, row.lifecycle_generation, false)
                .is_err());
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancellation_metadata_ack_does_not_retire_unproven_preparation() {
        use crate::acp::runner_lifecycle::{
            LifecycleTable, NativeResume, PreparationAuthorization, ResumeKind, StopDecision,
        };
        use std::sync::Mutex;

        for early_ack in [true, false] {
            let temporary = tempfile::TempDir::new_in("/tmp").unwrap();
            let _app_dir = super::super::test_support::isolate_app_dir_at(temporary.path());
            super::super::create_profile("ack").unwrap();
            let storage = Storage::open_unwatched("ack").unwrap();
            let mut row = Instance::new("ack", temporary.path().to_str().unwrap());
            row.source_profile = "ack".into();
            row.view = super::super::View::Structured;
            row.active_execution = Some(crate::session::instance::ActiveExecution {
                launch_id: Uuid::new_v4().to_string(),
                binding: crate::session::ExecutionBinding {
                    agent: "claude".into(),
                    stores: Vec::new(),
                    configuration: Vec::new(),
                    exported_default_store: None,
                    cwd: row.project_path.clone().into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                },
                capture: None,
                container: None,
            });
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            let original = capture_unique_origin(&row.id).unwrap();
            let lifecycle = Mutex::new(LifecycleTable::new(1));
            let lease = lifecycle
                .lock()
                .unwrap()
                .admit(&row.id, ResumeKind::Spawn)
                .unwrap();
            let issued = lifecycle.lock().unwrap().execution_admission(&lease);
            issued.set_origin(original.clone()).unwrap();
            let job = issued.begin_job();
            let (prepared, preparation) = original
                .prepare(&NativeResume::Spawn, &issued, |commit| {
                    PreparationAuthorization::acquire(
                        lifecycle.lock().unwrap(),
                        &lease,
                        &original,
                        false,
                        commit,
                    )
                })
                .unwrap();
            issued
                .set_prepared_origin(prepared.clone(), preparation)
                .unwrap();
            let mut retirement = issued.preparation_retirement().unwrap();
            let preparations = storage
                .load()
                .unwrap()
                .remove(0)
                .runner_journal
                .preparations;
            let stop = reserve_stop_from_origin(prepared, false).unwrap();
            assert!(matches!(
                lifecycle
                    .lock()
                    .unwrap()
                    .begin_owned_stop(&stop, "user_stopped"),
                StopDecision::CancelRequested
            ));
            let acknowledge = || {
                stop.update_projection(
                    |row| {
                        row.view = super::super::View::Terminal;
                        row.active_execution = None;
                        Ok(())
                    },
                    Ok,
                )
                .unwrap()
            };
            if early_ack {
                acknowledge();
            }
            assert!(require_quiescent(JournalScope::Stop(stop.clone()))
                .await
                .is_err());
            drop(job);
            assert!(tokio::time::timeout(
                Duration::from_secs(5),
                PreparationCustody::await_retired(retirement.clone()),
            )
            .await
            .unwrap()
            .is_err());
            assert_eq!(*retirement.borrow_and_update(), Some(false));
            assert!(issued.cancellation_observed());
            assert!(issued.check_active().is_err());
            assert!(lifecycle.lock().unwrap().abandon(&lease));
            if !early_ack {
                acknowledge();
            }
            let canonical = capture_unique_origin(&row.id).unwrap();
            assert!(stop.cancellation_origin().same_scope(&canonical));
            assert!(!stop.original().same_scope(&canonical));
            let before = std::fs::read(storage.sessions_path()).unwrap();
            assert!(settle(stop.clone()).await.is_err());
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
            let retained = storage.load().unwrap().remove(0);
            assert_eq!(retained.runner_journal.preparations, preparations);
            assert!(retained
                .lifecycle_reservation_is_owned(LifecycleOperation::Stop, stop.generation()));
        }
    }

    #[test]
    #[serial_test::serial]
    fn checkout_stop_refuses_fresh_durable_waiting_without_claiming() {
        let temporary = tempfile::TempDir::new_in("/tmp").unwrap();
        let _app_dir = super::super::test_support::isolate_app_dir_at(temporary.path());
        super::super::create_profile("waiting").unwrap();
        let storage = Storage::open_unwatched("waiting").unwrap();
        let mut expected = Instance::new("waiting", temporary.path().to_str().unwrap());
        expected.status = super::super::Status::Idle;
        storage
            .update(|rows, _| {
                rows.push(expected.clone());
                Ok(())
            })
            .unwrap();
        storage
            .update(|rows, _| {
                rows[0].status = super::super::Status::Waiting;
                Ok(())
            })
            .unwrap();
        assert!(reserve_owned_stop(&storage, &expected, true).is_err());
        let current = storage.load().unwrap().remove(0);
        assert_eq!(current.lifecycle_generation, expected.lifecycle_generation);
        assert!(current.lifecycle_reservation.is_none());
        assert_eq!(current.project_path, expected.project_path);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn launch_origin_rejects_transient_stop_and_profile_replacement() {
        let temporary = tempfile::TempDir::new_in("/tmp").unwrap();
        let _app_dir = super::super::test_support::isolate_app_dir_at(temporary.path());
        super::super::create_profile("origin").unwrap();
        let storage = Storage::open_unwatched("origin").unwrap();
        let row = Instance::new("origin", temporary.path().to_str().unwrap());
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let origin = std::sync::Arc::new(
            LaunchOrigin::capture_baseline_at(&row, std::sync::Arc::new(storage.clone())).unwrap(),
        );
        storage
            .update(|rows, _| {
                let row = &mut rows[0];
                let generation = row.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Stop,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )?;
                assert!(row
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Stop, generation));
                Ok(())
            })
            .unwrap();
        assert!(
            origin.validate().is_err(),
            "released Stop must still invalidate a delayed launch"
        );
        drop(origin);
        let current = storage.load().unwrap().remove(0);
        let stop = reserve_owned_stop(&storage, &current, false).unwrap();
        settle(stop.clone()).await.unwrap();
        release_owned_stop(&stop).unwrap();
        let fresh_origin = std::sync::Arc::new(
            LaunchOrigin::capture_baseline_at(
                &storage.load().unwrap().remove(0),
                std::sync::Arc::new(storage.clone()),
            )
            .unwrap(),
        );
        let profile_dir = storage.sessions_path().parent().unwrap();
        let replacement_rows = std::fs::read(storage.sessions_path()).unwrap();
        std::fs::rename(profile_dir, temporary.path().join("retired-profile")).unwrap();
        super::super::create_profile("origin").unwrap();
        let replacement = Storage::open_unwatched("origin").unwrap();
        std::fs::write(replacement.sessions_path(), replacement_rows).unwrap();
        assert!(
            fresh_origin.validate().is_err(),
            "copied rows must not adopt a replacement incarnation"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn last_issued_job_keeps_unproven_preparation_and_stop_generation() {
        use crate::acp::runner_lifecycle::{AdmissionRetirement, LifecycleTable, ResumeKind};
        use std::sync::Arc;
        let temporary = tempfile::tempdir().unwrap();
        let _home = super::super::test_support::isolate_app_dir_at(temporary.path());
        let storage = Storage::new_unwatched("preparing").unwrap();
        let row = Instance::new("preparing", temporary.path().to_str().unwrap());
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let lifecycle = Arc::new(std::sync::Mutex::new(LifecycleTable::new(0)));
        let lease = lifecycle
            .lock()
            .unwrap()
            .admit(&row.id, ResumeKind::Spawn)
            .unwrap();
        let issued = lifecycle.lock().unwrap().execution_admission(&lease);
        let origin =
            Arc::new(LaunchOrigin::capture_baseline_at(&row, Arc::new(storage.clone())).unwrap());
        issued.set_origin(origin.clone()).unwrap();
        let first = issued.begin_job();
        let last = issued.begin_job();
        let (prepared, preparation) = origin
            .prepare(
                &crate::acp::runner_lifecycle::NativeResume::Spawn,
                &issued,
                |commit| {
                    crate::acp::runner_lifecycle::PreparationAuthorization::acquire(
                        lifecycle.lock().unwrap(),
                        &lease,
                        &origin,
                        false,
                        commit,
                    )
                },
            )
            .unwrap();
        issued.set_prepared_origin(prepared, preparation).unwrap();
        let retirement = issued.preparation_retirement().unwrap();
        let claimed = storage.load().unwrap().remove(0);
        let preparations = claimed.runner_journal.preparations.clone();
        let before = std::fs::read(storage.sessions_path()).unwrap();
        assert!(reserve_owned_stop(&storage, &claimed, true).is_err());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        let stop_generation = reserve_owned_stop(&storage, &claimed, false).unwrap();
        assert!(origin.validate().is_err());
        issued.retire(AdmissionRetirement {
            lease,
            lifecycle: lifecycle.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            execution: None,
        });
        drop(first);
        assert!(lifecycle.lock().unwrap().is_owned(&row.id));
        assert!(
            require_quiescent(JournalScope::Stop(stop_generation.clone()))
                .await
                .is_err()
        );
        assert_eq!(
            storage.load().unwrap()[0].runner_journal.preparations.len(),
            1
        );
        let workspace = super::super::acquire_session_workspace_claim_lock().unwrap();
        drop(last);
        let pending = {
            let mut closing = lifecycle.lock().unwrap();
            assert!(!closing.is_owned(&row.id));
            let pending = closing.close_admissions();
            for kind in [ResumeKind::Spawn, ResumeKind::Attach] {
                assert_eq!(
                    closing.admit("future-admission", kind),
                    Err(crate::acp::runner_lifecycle::AdmitError::ShuttingDown)
                );
            }
            pending
        };
        let original = pending
            .into_iter()
            .next()
            .expect("original released preparation");
        let drain = original.drain();
        tokio::pin!(drain);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(matches!(
                std::future::Future::poll(drain.as_mut(), cx),
                std::task::Poll::Pending
            )))
            .await
        );
        assert_eq!(*retirement.borrow(), None);
        drop(workspace);
        assert!(drain.await.is_err());
        assert_eq!(*retirement.borrow(), Some(false));
        assert!(!lifecycle.lock().unwrap().is_owned(&row.id));
        let before = std::fs::read(storage.sessions_path()).unwrap();
        assert!(settle(stop_generation.clone()).await.is_err());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        let final_row = storage.load().unwrap().remove(0);
        assert_eq!(final_row.runner_journal.preparations, preparations);
        assert!(final_row.lifecycle_reservation_is_owned(
            LifecycleOperation::Stop,
            stop_generation.generation()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn authenticated_endpoint_cleanup_preserves_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let path = temporary.path().join("owned.stop");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let metadata = SocketEndpointIdentity::observe(&path).unwrap();
        crate::process::worker_registry::retire_endpoint("cleanup-original", &path, &metadata)
            .unwrap();
        assert!(!path.exists());
        drop(listener);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let metadata = SocketEndpointIdentity::observe(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        crate::process::worker_registry::retire_endpoint("cleanup-original", &path, &metadata)
            .unwrap();
        assert!(
            path.exists(),
            "a replacement socket must retain its pathname"
        );
        drop((listener, replacement));
    }

    #[test]
    #[serial_test::serial]
    fn stop_admission_rejects_same_generation_plan_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let _home = super::super::test_support::isolate_app_dir_at(temporary.path());
        let storage = Storage::new_unwatched("plan").unwrap();
        let original = Instance::new("original", "/tmp/original-stop-plan");
        storage
            .update(|rows, _| {
                rows.push(original.clone());
                Ok(())
            })
            .unwrap();
        storage
            .update(|rows, _| {
                rows[0].command = "changed-command".into();
                Ok(())
            })
            .unwrap();
        let before = std::fs::read(storage.sessions_path()).unwrap();
        assert!(reserve_owned_stop(&storage, &original, true).is_err());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        let current = storage.load().unwrap().remove(0);
        let generation = reserve_owned_stop(&storage, &current, true).unwrap();
        assert!(generation.generation() > original.lifecycle_generation);
        release_owned_stop(&generation).unwrap();
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.lifecycle_generation, generation.generation());
        assert!(stored.lifecycle_reservation.is_none());
    }
}

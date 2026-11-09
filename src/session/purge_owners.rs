//! Frozen cleanup ownership for failed purges and non-destructive forgetting.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::anchored_fs::ResolvedDataFile;
use super::deletion::DeletionRequest;
use super::path_identity::CleanupProtection;
use super::storage::{
    acquire_open_storage_flock, app_dir_for_profile_dir, try_acquire_storage_flock,
};
use super::{AnchoredDir, Instance, Storage};
use crate::acp::runner_lifecycle::PrimeOwnership;

pub(crate) const FILE_NAME: &str = "pending-purge-owners.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RunnerCapture {
    CancelledBeforeBirth {
        producer_epoch: u64,
        producer_id: uuid::Uuid,
        scopes: Vec<crate::process::worker_registry::ScopeOwnership>,
    },
    Uncaptured {
        scopes: Vec<crate::process::worker_registry::ScopeOwnership>,
        producer_epoch: Option<u64>,
        producer_id: Option<uuid::Uuid>,
        prime: PrimeOwnership,
    },
    Captured {
        pid: u32,
        generation: u64,
        #[serde(deserialize_with = "serde::Deserialize::deserialize")]
        birth_fingerprint: Option<crate::process::ProcessBirthFingerprint>,
        prime: PrimeOwnership,
        scopes: Vec<crate::process::worker_registry::ScopeOwnership>,
    },
    Unresolved {
        error: String,
    },
}

impl RunnerCapture {
    pub(crate) fn ensure_stopped(&self, session_id: Option<&str>) -> Result<()> {
        self.ensure_stopped_with(
            session_id,
            crate::process::worker_registry::load_strict,
            crate::process::worker::is_process_group_alive,
            peer_pid_for,
        )
    }

    fn ensure_stopped_with(
        &self,
        session_id: Option<&str>,
        load: impl FnOnce(&str) -> Result<Option<crate::process::worker_registry::WorkerRecord>>,
        process_group_alive: impl Fn(u32) -> bool,
        peer_pid: impl Fn(&str) -> Result<Option<u32>>,
    ) -> Result<()> {
        let Self::Unresolved { error } = self else {
            anyhow::ensure!(
                !matches!(
                    self,
                    Self::Uncaptured {
                        producer_epoch: Some(_),
                        ..
                    }
                ),
                "Producer completion has not been durably captured; purge resources retained"
            );
            let Some(session_id) = session_id else {
                anyhow::bail!("Runner session identity is missing; purge resources retained");
            };
            let scopes = match self {
                Self::Captured { scopes, .. }
                | Self::Uncaptured { scopes, .. }
                | Self::CancelledBeforeBirth { scopes, .. } => scopes,
                Self::Unresolved { .. } => unreachable!(),
            };
            for scope in scopes {
                anyhow::ensure!(
                    crate::process::systemd::scope_stopped(session_id, scope)?,
                    "Captured runner scope has not exited; purge resources retained"
                );
            }
            match self {
                Self::Captured { prime, .. } | Self::Uncaptured { prime, .. } => {
                    prime.ensure_stopped()?
                }
                Self::CancelledBeforeBirth { .. } => {}
                Self::Unresolved { .. } => unreachable!(),
            }
            let record = load(session_id)
                .map_err(|error| anyhow::anyhow!("worker registry is unreadable: {error:#}"))?;
            let peer_pid = peer_pid(session_id).map_err(|error| {
                anyhow::anyhow!("runner socket identity is unreadable: {error:#}")
            })?;
            let Self::Captured {
                pid,
                generation,
                birth_fingerprint,
                ..
            } = self
            else {
                anyhow::ensure!(
                    record.is_none() && peer_pid.is_none(),
                    "Runner ownership changed after capture; purge resources retained"
                );
                return Ok(());
            };
            anyhow::ensure!(
                birth_fingerprint.is_some() && *generation != 0,
                "Captured runner incarnation is unattested; purge resources retained"
            );
            let alive = process_group_alive(*pid);
            match record {
                None => {
                    anyhow::ensure!(
                        !alive && peer_pid.is_none(),
                        "Captured runner {pid} has not exited; purge resources retained"
                    );
                }
                Some(record) => {
                    let identity = crate::acp::runner_lifecycle::RunnerIdentity {
                        pid: *pid,
                        generation: *generation,
                        birth_fingerprint: *birth_fingerprint,
                    };
                    anyhow::ensure!(
                        identity.owns_record(&record),
                        "Captured runner identity was replaced (pid {} generation {}); purge resources retained",
                        record.pid,
                        record.generation
                    );
                    anyhow::ensure!(
                        !alive
                            && !crate::process::worker_registry::is_record_live(&record)
                            && peer_pid.is_none_or(|peer| peer == *pid),
                        "Captured runner {pid} has not exited; purge resources retained"
                    );
                }
            }
            return Ok(());
        };
        anyhow::bail!("Runner ownership unresolved; purge resources retained: {error}")
    }

    pub(crate) fn bind_retirement(
        &mut self,
        target: &crate::acp::supervisor::RetirementTarget,
    ) -> Result<()> {
        if let RunnerCapture::Unresolved { error } = &*self {
            anyhow::bail!("Runner ownership unresolved; purge resources retained: {error}");
        }
        let (identity, ownership) = if target.identity.is_some() {
            (target.identity, target.prime.clone())
        } else {
            target
                .lease
                .as_ref()
                .map(crate::acp::runner_lifecycle::Lease::constructed_ownership)
                .unwrap_or_else(|| (None, target.prime.clone()))
        };
        let mut scopes = target.scopes.clone();
        if let Some(lease) = &target.lease {
            for scope in lease.scopes() {
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }
        *self = match (&*self, identity) {
            (
                RunnerCapture::Captured {
                    pid,
                    generation,
                    scopes: old,
                    birth_fingerprint,
                    prime,
                },
                Some(identity),
            ) => {
                anyhow::ensure!(
                    *pid == identity.pid
                        && *generation == identity.generation
                        && *birth_fingerprint == identity.birth_fingerprint
                        && *prime == ownership,
                    "Captured runner was replaced before purge commit"
                );
                for scope in old {
                    if !scopes.contains(scope) {
                        scopes.push(scope.clone());
                    }
                }
                RunnerCapture::Captured {
                    pid: *pid,
                    generation: *generation,
                    birth_fingerprint: *birth_fingerprint,
                    prime: prime.clone(),
                    scopes,
                }
            }
            (RunnerCapture::Captured { .. }, None) => {
                anyhow::bail!("Captured runner identity disappeared before purge commit")
            }
            (_, Some(identity)) => RunnerCapture::Captured {
                pid: identity.pid,
                generation: identity.generation,
                birth_fingerprint: identity.birth_fingerprint,
                prime: ownership,
                scopes,
            },
            (_, None) => RunnerCapture::Uncaptured {
                scopes,
                prime: ownership,
                producer_epoch: target
                    .lease
                    .as_ref()
                    .map(crate::acp::runner_lifecycle::Lease::epoch),
                producer_id: target
                    .lease
                    .as_ref()
                    .map(crate::acp::runner_lifecycle::Lease::producer_id),
            },
        };
        Ok(())
    }

    pub(crate) fn ensure_retirement_target(
        &self,
        target: &mut crate::acp::supervisor::RetirementTarget,
    ) -> Result<()> {
        let (observed, observed_prime) = if target.identity.is_some() {
            (target.identity, target.prime.clone())
        } else {
            target
                .lease
                .as_ref()
                .map(crate::acp::runner_lifecycle::Lease::constructed_ownership)
                .unwrap_or_else(|| (None, target.prime.clone()))
        };
        let scopes = match self {
            RunnerCapture::CancelledBeforeBirth {
                producer_epoch,
                producer_id,
                scopes,
            } => {
                anyhow::ensure!(
                    observed.is_none(),
                    "A runner appeared after completed producer capture"
                );
                if let Some(lease) = &target.lease {
                    anyhow::ensure!(
                        lease.epoch() == *producer_epoch
                            && lease.producer_id() == *producer_id
                            && lease.was_cancelled_before_birth()
                            && observed_prime == PrimeOwnership::NotPrime,
                        "A replacement producer appeared after completed capture"
                    );
                }
                target.identity = None;
                target.prime = PrimeOwnership::NotPrime;
                scopes
            }
            RunnerCapture::Captured {
                pid,
                generation,
                scopes,
                birth_fingerprint,
                prime,
            } => {
                anyhow::ensure!(
                    birth_fingerprint.is_some()
                        && *generation != 0
                        && *prime != PrimeOwnership::Unresolved,
                    "Captured runner authority remains unresolved; purge resources retained"
                );
                if let Some(identity) = observed {
                    anyhow::ensure!(
                        identity.pid == *pid
                            && identity.generation == *generation
                            && identity.birth_fingerprint == *birth_fingerprint
                            && observed_prime == *prime,
                        "Captured runner was replaced before retirement; purge resources retained"
                    );
                } else {
                    anyhow::ensure!(
                        target.lease.is_none(),
                        "A replacement producer appeared before recovery; purge resources retained"
                    );
                }
                target.identity = Some(crate::acp::runner_lifecycle::RunnerIdentity {
                    pid: *pid,
                    generation: *generation,
                    birth_fingerprint: *birth_fingerprint,
                });
                target.prime = prime.clone();
                scopes
            }
            RunnerCapture::Uncaptured {
                scopes,
                producer_epoch,
                producer_id,
                prime,
            } => {
                anyhow::ensure!(
                    observed.is_none_or(|identity| Some(identity.generation) == *producer_epoch)
                        && target
                            .lease
                            .as_ref()
                            .is_none_or(|lease| Some(lease.epoch()) == *producer_epoch
                                && Some(lease.producer_id()) == *producer_id),
                    "Uncaptured runner was replaced before retirement; purge resources retained"
                );
                let live_producer = producer_epoch.is_some()
                    && target.lease.as_ref().is_some_and(|lease| {
                        Some(lease.epoch()) == *producer_epoch
                            && Some(lease.producer_id()) == *producer_id
                    });
                anyhow::ensure!(
                    producer_epoch.is_none() || live_producer,
                    "Captured producer lifetime remains unresolved; purge resources retained"
                );
                anyhow::ensure!(
                    (*prime != PrimeOwnership::Unresolved && (observed.is_none() || observed_prime == *prime))
                        || (*prime == PrimeOwnership::Unresolved && live_producer),
                    "Captured native ownership changed or remains unresolved; purge resources retained"
                );
                target.prime = if live_producer && *prime == PrimeOwnership::Unresolved {
                    observed_prime
                } else {
                    prime.clone()
                };
                target.identity = observed;
                scopes
            }
            RunnerCapture::Unresolved { error } => {
                anyhow::bail!("Runner ownership unresolved; purge resources retained: {error}")
            }
        };
        for scope in scopes {
            anyhow::ensure!(
                *scope != crate::process::worker_registry::ScopeOwnership::Unresolved,
                "Captured scope ownership unresolved; purge resources retained"
            );
            if !target.scopes.contains(scope) {
                target.scopes.push(scope.clone());
            }
        }
        target.journalled = true;
        Ok(())
    }

    pub(crate) fn completed(
        &self,
        target: &crate::acp::supervisor::RetirementTarget,
    ) -> Result<Self> {
        let RunnerCapture::Uncaptured {
            producer_epoch: Some(epoch),
            producer_id: Some(producer_id),
            scopes,
            ..
        } = self
        else {
            return Ok(self.clone());
        };
        let lease = target
            .lease
            .as_ref()
            .filter(|lease| lease.epoch() == *epoch && lease.producer_id() == *producer_id)
            .context(
                "The captured live producer is no longer available; purge resources retained",
            )?;
        let (identity, prime) = lease.constructed_ownership();
        anyhow::ensure!(
            prime != PrimeOwnership::Unresolved,
            "Completed producer native ownership is unresolved"
        );
        let mut scopes = scopes.clone();
        for scope in target.scopes.iter().cloned().chain(lease.scopes()) {
            anyhow::ensure!(
                scope != crate::process::worker_registry::ScopeOwnership::Unresolved,
                "Completed producer scope ownership is unresolved"
            );
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        let runner = match identity {
            Some(identity) => {
                anyhow::ensure!(
                    identity.generation == *epoch && identity.birth_fingerprint.is_some(),
                    "Completed producer runner incarnation is unattested"
                );
                RunnerCapture::Captured {
                    pid: identity.pid,
                    generation: identity.generation,
                    birth_fingerprint: identity.birth_fingerprint,
                    prime,
                    scopes,
                }
            }
            None => {
                anyhow::ensure!(
                    lease.was_cancelled_before_birth() && prime == PrimeOwnership::NotPrime,
                    "A missing runner does not prove that its producer built no child"
                );
                RunnerCapture::CancelledBeforeBirth {
                    producer_epoch: *epoch,
                    producer_id: *producer_id,
                    scopes,
                }
            }
        };
        Ok(runner)
    }
}

fn peer_pid_for(session_id: &str) -> Result<Option<u32>> {
    let base = crate::process::worker_registry::socket_path_for(session_id)?;
    let control = crate::process::worker::control_socket_sibling(&base);
    let peer_pid = crate::process::worker::peer_pid_from_socket(&control)
        .or_else(|| crate::process::worker::peer_pid_from_socket(&base));
    if peer_pid.is_none()
        && [control.as_path(), base.as_path()]
            .into_iter()
            .any(|path| std::fs::symlink_metadata(path).is_ok())
    {
        anyhow::bail!("runner socket exists but its peer identity could not be proven");
    }
    Ok(peer_pid)
}

pub(crate) fn capture_runner(session_id: &str) -> RunnerCapture {
    match crate::process::worker_registry::load_strict(session_id) {
        Ok(Some(record)) if record.pid > 0 && i32::try_from(record.pid).is_ok() => {
            RunnerCapture::Captured {
                pid: record.pid,
                generation: record.generation,
                birth_fingerprint: crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                    &record,
                )
                .birth_fingerprint,
                prime: PrimeOwnership::from_record(&record),
                scopes: vec![record.scope],
            }
        }
        Ok(Some(_)) => RunnerCapture::Unresolved {
            error: "Invalid captured runner pid".into(),
        },
        Ok(None) => match peer_pid_for(session_id) {
            Ok(None) => RunnerCapture::Uncaptured {
                scopes: Vec::new(),
                producer_epoch: None,
                producer_id: None,
                prime: PrimeOwnership::NotPrime,
            },
            Ok(Some(pid)) => RunnerCapture::Unresolved {
                error: format!("registry absent but runner socket peer {pid} is active"),
            },
            Err(error) => RunnerCapture::Unresolved {
                error: format!("runner socket probe failed: {error:#}"),
            },
        },
        Err(error) => RunnerCapture::Unresolved {
            error: format!("{error:#}"),
        },
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OwnerPurpose {
    Purge,
    Retained {
        instance: Box<Instance>,
        terminal_scopes: Option<Vec<crate::process::worker_registry::ScopeOwnership>>,
    },
}

#[derive(Serialize, Deserialize)]
struct Owner {
    token: String,
    session_id: String,
    profile: String,
    generation: u64,
    protection: CleanupProtection,
    runner: RunnerCapture,
    #[serde(default)]
    request: Option<DeletionRequest>,
    #[serde(default)]
    additional_protection: Option<CleanupProtection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<OwnerPurpose>,
}

pub(super) struct RecoveryPlan {
    pub(super) token: String,
    pub(super) session_id: String,
    pub(super) profile: String,
    pub(super) request: Option<DeletionRequest>,
    pub(super) additional_protection: Option<CleanupProtection>,
    pub(super) protection: CleanupProtection,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    owners: Vec<Owner>,
}

pub(crate) struct CleanupOwners(Journal);

impl CleanupOwners {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        Ok(Self(decode(open(root)?.read()?.as_deref())?))
    }

    pub(crate) fn load_for(storage: &Storage) -> Result<Self> {
        Ok(Self(decode(open_for(storage)?.read()?.as_deref())?))
    }

    pub(crate) fn retains_session(&self, session_id: &str) -> bool {
        retains_session(&self.0, session_id)
    }

    pub(crate) fn session_ids(&self) -> impl Iterator<Item = &str> {
        self.0.owners.iter().map(|owner| owner.session_id.as_str())
    }

    pub(crate) fn frozen_instances(&self) -> impl Iterator<Item = &Instance> {
        self.0
            .owners
            .iter()
            .filter_map(|owner| match &owner.purpose {
                Some(OwnerPurpose::Retained { instance, .. }) => Some(instance.as_ref()),
                Some(OwnerPurpose::Purge) => {
                    owner.request.as_ref().map(|request| &request.instance)
                }
                None => None,
            })
    }

    pub(crate) fn blocks_publisher(
        &self,
        instance_id: &str,
        source: Option<&super::ExecutionBinding>,
    ) -> bool {
        self.0.owners.iter().any(|owner| match &owner.purpose {
            Some(OwnerPurpose::Retained { instance, .. }) => {
                instance.id == instance_id || publisher_overlaps(instance, source)
            }
            _ => false,
        })
    }

    pub(crate) fn blocks_conversation(
        &self,
        instance_id: &str,
        session_id: &str,
        source: Option<&super::ExecutionBinding>,
    ) -> bool {
        let owns = |id: Option<&str>, binding: Option<&super::ConversationBinding>| {
            id == Some(session_id)
                && match (source, binding.filter(|binding| binding.is_known())) {
                    (Some(source), Some(owner)) => owner
                        .execution
                        .as_ref()
                        .is_none_or(|owner| namespaces_overlap(owner, source)),
                    _ => true,
                }
        };
        self.0.owners.iter().any(|owner| {
            let retained = matches!(owner.purpose, Some(OwnerPurpose::Retained { .. }));
            let peer = match &owner.purpose {
                Some(OwnerPurpose::Retained { instance, .. }) => instance.as_ref(),
                Some(OwnerPurpose::Purge) => match &owner.request {
                    Some(request) => &request.instance,
                    None => return false,
                },
                None => return false,
            };
            if retained && (peer.id == instance_id || publisher_overlaps(peer, source)) {
                return true;
            }
            (retained || peer.id != instance_id)
                && conversation_inventory(peer)
                    .into_iter()
                    .any(|(id, binding)| owns(Some(id), binding))
        })
    }

    pub(crate) fn blocks_acp_import(
        &self,
        instance_id: &str,
        session_id: &str,
        source: Option<&super::ExecutionBinding>,
    ) -> bool {
        self.frozen_instances().any(|row| {
            row.acp_session_id.as_deref() == Some(session_id)
                || row
                    .prior_tool_session_ids
                    .values()
                    .any(|parked| parked.acp_session_id.as_deref() == Some(session_id))
        }) || self.blocks_conversation(instance_id, session_id, source)
    }

    pub(crate) fn protections(&self) -> impl Iterator<Item = &CleanupProtection> {
        self.0.owners.iter().flat_map(|owner| {
            std::iter::once(&owner.protection).chain(owner.additional_protection.as_ref())
        })
    }
}

fn conversation_inventory(row: &Instance) -> Vec<(&str, Option<&super::ConversationBinding>)> {
    let claude_binding = |binding: Option<&super::ConversationBinding>| {
        binding
            .filter(|binding| binding.is_known())
            .and_then(|binding| binding.execution.as_ref())
            .is_some_and(|execution| execution.agent == "claude")
    };
    let mut ids = Vec::new();
    if let Some(id) = row.agent_session_id.as_deref() {
        ids.push((id, row.agent_session_binding.as_ref()));
    }
    if let Some((id, binding, _)) = row.conversation_target() {
        ids.push((id, binding));
    }
    if row.tool == "claude"
        || row
            .active_execution
            .as_ref()
            .is_some_and(|active| active.binding.agent == "claude")
        || claude_binding(row.agent_session_binding.as_ref())
    {
        if let Some(id) = row.acp_session_id.as_deref() {
            ids.push((id, row.agent_session_binding.as_ref()));
        }
    }
    for (tool, parked) in &row.prior_tool_session_ids {
        if let Some(id) = parked.agent_session_id.as_deref() {
            ids.push((id, parked.agent_session_binding.as_ref()));
        }
        if tool == "claude" || claude_binding(parked.agent_session_binding.as_ref()) {
            if let Some(id) = parked.acp_session_id.as_deref() {
                ids.push((id, parked.agent_session_binding.as_ref()));
            }
        }
    }
    ids
}

fn publisher_overlaps(row: &Instance, source: Option<&super::ExecutionBinding>) -> bool {
    let Some(source) = source else {
        return true;
    };
    let Some(active) = row.active_execution.as_ref() else {
        return true;
    };
    namespaces_overlap(&active.binding, source)
        || row.prior_tool_session_ids.values().any(|parked| {
            if parked.agent_session_id.is_none() && parked.acp_session_id.is_none() {
                return false;
            }
            parked
                .agent_session_binding
                .as_ref()
                .filter(|binding| binding.is_known())
                .and_then(|binding| binding.execution.as_ref())
                .is_none_or(|owner| namespaces_overlap(owner, source))
        })
}

fn namespaces_overlap(owner: &super::ExecutionBinding, source: &super::ExecutionBinding) -> bool {
    if owner.agent.is_empty()
        || source.agent.is_empty()
        || owner.filesystem.is_empty()
        || source.filesystem.is_empty()
    {
        return true;
    }
    if owner.agent != source.agent || owner.filesystem != source.filesystem {
        return false;
    }
    if owner.stores.is_empty() || source.stores.is_empty() {
        return true;
    }
    owner.stores.iter().any(|left| {
        source.stores.iter().any(|right| {
            if owner.filesystem == "host" {
                match (std::fs::metadata(left), std::fs::metadata(right)) {
                    (Ok(left), Ok(right)) => {
                        crate::process::metadata_identity(&left)
                            == crate::process::metadata_identity(&right)
                    }
                    (left_metadata, right_metadata)
                        if left_metadata
                            .as_ref()
                            .err()
                            .is_none_or(|error| error.kind() == std::io::ErrorKind::NotFound)
                            && right_metadata.as_ref().err().is_none_or(|error| {
                                error.kind() == std::io::ErrorKind::NotFound
                            }) =>
                    {
                        match (
                            super::capture::canonicalize_allowing_missing_leaf(left),
                            super::capture::canonicalize_allowing_missing_leaf(right),
                        ) {
                            (Some(left), Some(right)) => left == right,
                            _ => true,
                        }
                    }
                    _ => true,
                }
            } else {
                crate::git::template::lexical_normalize(left)
                    == crate::git::template::lexical_normalize(right)
            }
        })
    })
}

#[derive(Debug, thiserror::Error)]
#[error("Native conversation ownership is busy")]
pub(crate) struct ConversationOwnershipBusy;

#[derive(Debug, thiserror::Error)]
#[error("Native conversation is protected by retained cleanup ownership")]
pub(crate) struct ConversationOwnershipRetained;

pub(crate) struct ConversationReadLock {
    _namespace: std::fs::File,
    _file: Option<std::fs::File>,
}

impl ConversationReadLock {
    pub(crate) fn namespace(storage: &Storage) -> Result<Self> {
        Ok(Self {
            _namespace: lock_namespace(storage, false)?,
            _file: None,
        })
    }

    pub(crate) fn conversation(
        &self,
        storage: &Storage,
        session_id: &str,
    ) -> Result<std::fs::File> {
        lock_conversation(storage, session_id, false)
    }

    pub(crate) fn acquire(storage: &Storage, session_id: &str) -> Result<Self> {
        Ok(Self {
            _namespace: lock_namespace(storage, false)?,
            _file: Some(lock_conversation(storage, session_id, false)?),
        })
    }
}

// Callers can hold lifecycle and registry locks; never wait here.
fn lock_conversation(
    storage: &Storage,
    session_id: &str,
    exclusive: bool,
) -> Result<std::fs::File> {
    use std::fmt::Write;
    let profile = storage
        .sessions_path()
        .parent()
        .context("profile directory missing")?;
    let root = AnchoredDir::open(app_dir_for_profile_dir(profile))?;
    const PREFIX: &str = ".native-conversation-";
    let digest = Sha256::digest(session_id.as_bytes());
    let mut name = String::with_capacity(PREFIX.len() + digest.len() * 2);
    name.push_str(PREFIX);
    for byte in digest {
        write!(&mut name, "{byte:02x}").expect("writing to String");
    }
    let (file, _) = root.bind_file(name.as_ref())?.open_sidecar()?;
    let result = if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)
    } else {
        fs2::FileExt::try_lock_shared(&file)
    };
    if let Err(error) = result {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(ConversationOwnershipBusy.into());
        }
        return Err(error.into());
    }
    Ok(file)
}

fn lock_namespace(storage: &Storage, exclusive: bool) -> Result<std::fs::File> {
    let profile = storage
        .sessions_path()
        .parent()
        .context("profile directory missing")?;
    let root = AnchoredDir::open(app_dir_for_profile_dir(profile))?;
    let (file, _) = root
        .bind_file(".retained-publisher-admission".as_ref())?
        .open_sidecar()?;
    let result = if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)
    } else {
        fs2::FileExt::try_lock_shared(&file)
    };
    match result {
        Ok(()) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(ConversationOwnershipBusy.into())
        }
        Err(error) => Err(error.into()),
    }
}

fn open(root: &Path) -> Result<ResolvedDataFile> {
    AnchoredDir::open(root)?.bind_file(FILE_NAME.as_ref())
}

fn open_for(storage: &Storage) -> Result<ResolvedDataFile> {
    let profile = storage
        .sessions_path()
        .parent()
        .context("profile directory missing")?;
    open(app_dir_for_profile_dir(profile))
}

fn decode(content: Option<&str>) -> Result<Journal> {
    decode_version(content, 4)
}

fn decode_version(content: Option<&str>, version: u32) -> Result<Journal> {
    let content = content.context("Pending purge ownership journal is missing")?;
    let journal =
        serde_json::from_str::<Journal>(content).context("reading pending purge ownership")?;
    anyhow::ensure!(
        journal.version == version,
        "Unsupported pending purge ownership version"
    );
    let mut tokens = HashSet::with_capacity(journal.owners.len());
    for owner in &journal.owners {
        anyhow::ensure!(
            uuid::Uuid::parse_str(&owner.token).is_ok()
                && tokens.insert(&owner.token)
                && super::is_valid_session_id(&owner.session_id)
                && !owner.profile.is_empty(),
            "Invalid pending purge owner"
        );
        match (version, &owner.purpose) {
            (3, None) | (4, Some(OwnerPurpose::Purge)) => {}
            (4, Some(OwnerPurpose::Retained { instance, .. })) => {
                anyhow::ensure!(
                    owner.request.is_none()
                        && owner.additional_protection.is_none()
                        && instance.id == owner.session_id
                        && (instance.source_profile.is_empty()
                            || instance.source_profile == owner.profile)
                        && instance.lifecycle_generation == owner.generation
                        && instance.has_pending_trash()
                        && instance.failed_purge_forced().is_none(),
                    "Invalid retained cleanup ownership"
                );
            }
            _ => anyhow::bail!("Invalid cleanup ownership purpose"),
        }
        owner.protection.validate()?;
        if let RunnerCapture::CancelledBeforeBirth {
            producer_epoch,
            producer_id,
            ..
        } = &owner.runner
        {
            anyhow::ensure!(
                *producer_epoch != 0 && producer_id.get_version_num() == 4,
                "Invalid completed producer identity"
            );
        }
        if let RunnerCapture::Uncaptured {
            producer_epoch,
            producer_id: Some(id),
            ..
        } = &owner.runner
        {
            anyhow::ensure!(
                producer_epoch.is_some_and(|epoch| epoch != 0) && id.get_version_num() == 4,
                "Invalid captured producer identity"
            );
        }
        if let RunnerCapture::Captured { pid, .. } = &owner.runner {
            anyhow::ensure!(
                *pid > 0 && i32::try_from(*pid).is_ok(),
                "Invalid captured runner pid"
            );
        }
        if let Some(request) = &owner.request {
            let source_profile_matches = request.instance.source_profile.is_empty()
                || request.instance.source_profile == owner.profile;
            anyhow::ensure!(
                request.session_id == owner.session_id
                    && request.instance.id == owner.session_id
                    && request.instance.lifecycle_generation == owner.generation
                    && source_profile_matches,
                "Pending purge request identity does not match its owner"
            );
        }
        if let Some(protection) = &owner.additional_protection {
            protection.validate()?;
        }
    }
    Ok(journal)
}

/// Sibling namespace scans accept an absent journal but refuse malformed ownership.
/// Destructive cleanup and recovery use the strict decoder instead.
fn decode_sibling(content: Option<&str>) -> Result<Journal> {
    match content {
        Some(_) => decode(content),
        None => Ok(Journal {
            version: 4,
            owners: Vec::new(),
        }),
    }
}

fn save(file: &ResolvedDataFile, journal: &Journal) -> Result<()> {
    file.replace(&serde_json::to_vec(journal)?)?;
    file.sync_parent()
}

#[derive(Debug, thiserror::Error)]
#[error("Profile '{profile}' has protected resources; resolve retained cleanup ownership before renaming or deleting it")]
pub(crate) struct ProfileHasPendingPurges {
    profile: String,
}

pub(crate) fn ensure_profile_has_no_pending_purges(root: &Path, profile: &str) -> Result<()> {
    let file = open(root)?;
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let journal = decode(file.read()?.as_deref())?;
    if journal.owners.iter().any(|owner| owner.profile == profile) {
        return Err(ProfileHasPendingPurges {
            profile: profile.to_owned(),
        }
        .into());
    }
    if journal.owners.is_empty() {
        return Ok(());
    }
    fn registry_files(root: &Path, profile: &str) -> Result<[ResolvedDataFile; 2]> {
        let parent = root.join("profiles").join(profile).canonicalize()?;
        let directory = AnchoredDir::open(&parent)?;
        Ok([
            directory.resolve_file(std::ffi::OsStr::new("sessions.json"))?,
            directory.resolve_file(std::ffi::OsStr::new("groups.json"))?,
        ])
    }
    let selected = registry_files(root, profile)?;
    let mut seen = HashSet::new();
    for owner in &journal.owners {
        if !seen.insert(owner.profile.as_str()) {
            continue;
        }
        for retained in registry_files(root, &owner.profile)? {
            for candidate in &selected {
                if retained.same_target(candidate)? {
                    return Err(ProfileHasPendingPurges {
                        profile: profile.to_owned(),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn initialize(root: &Path) -> Result<()> {
    let file = open(root)?;
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let content = file.read()?;
    match content.as_deref() {
        Some(content) => {
            upcast_journal(content, JournalInput::Migration)?;
        }
        None => save(
            &file,
            &Journal {
                version: 4,
                owners: Vec::new(),
            },
        )?,
    }
    Ok(())
}

/// Dropping this handle deliberately leaves its durable ownership intact.
pub(crate) struct PurgeOwner {
    file: ResolvedDataFile,
    token: String,
    runner: RunnerCapture,
    session_id: Option<String>,
}

pub(crate) struct PurgeCapture {
    terminal_scopes: Option<Vec<crate::process::worker_registry::ScopeOwnership>>,
    protection: CleanupProtection,
    runner: RunnerCapture,
    session_id: String,
}

pub(crate) enum JournalInput {
    Migration,
    CompleteBackup,
}

pub(crate) fn upcast_journal(
    content: &str,
    input: JournalInput,
) -> Result<std::borrow::Cow<'_, [u8]>> {
    let mut value: serde_json::Value =
        serde_json::from_str(content).context("Reading pending purge ownership backup")?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(3 | 4) => {
            if matches!(input, JournalInput::CompleteBackup) {
                let owners = value
                    .get("owners")
                    .and_then(serde_json::Value::as_array)
                    .context("Invalid pending purge ownership backup")?;
                anyhow::ensure!(
                    owners.iter().all(|owner| owner.get("request").is_some()
                        && owner.get("additional_protection").is_some()),
                    "Incomplete purge backup omits frozen cleanup request or protection"
                );
            }
            match value.get("version").and_then(serde_json::Value::as_u64) {
                Some(3) => {
                    decode_version(Some(content), 3)?;
                }
                Some(4) => {
                    decode(Some(content))?;
                }
                _ => unreachable!(),
            }
            return Ok(std::borrow::Cow::Borrowed(content.as_bytes()));
        }
        Some(2) => {}
        _ => anyhow::bail!("Unsupported pending purge ownership version"),
    }
    let owners = value["owners"]
        .as_array_mut()
        .context("Invalid pending purge ownership backup")?;
    for owner in owners {
        let owner = owner
            .as_object_mut()
            .context("Invalid pending purge owner")?;
        for field in ["request", "additional_protection"] {
            if !owner.contains_key(field) {
                owner.insert(field.to_owned(), serde_json::Value::Null);
            }
        }
        let runner = owner
            .get_mut("runner")
            .and_then(serde_json::Value::as_object_mut)
            .context("Invalid captured runner")?;
        match runner.get("state").and_then(serde_json::Value::as_str) {
            Some("captured" | "uncaptured") => {
                let captured =
                    runner.get("state").and_then(serde_json::Value::as_str) == Some("captured");
                if !captured {
                    runner
                        .entry("producer_id")
                        .or_insert(serde_json::Value::Null);
                }
                runner
                    .entry("prime")
                    .or_insert_with(|| serde_json::json!({"state":"unresolved"}));
                if captured {
                    runner
                        .entry("birth_fingerprint")
                        .or_insert(serde_json::Value::Null);
                }
                runner
                    .entry("scopes")
                    .or_insert_with(|| serde_json::json!([{"state":"unresolved"}]));
            }
            Some("unresolved") => {}
            Some("cancelled_before_birth") => {
                anyhow::bail!("Legacy purge capture cannot attest completed construction")
            }
            _ => anyhow::bail!("Invalid captured runner state"),
        }
    }
    value["version"] = serde_json::json!(3);
    let bytes = serde_json::to_vec(&value)?;
    decode_version(Some(std::str::from_utf8(&bytes)?), 3)?;
    Ok(std::borrow::Cow::Owned(bytes))
}

fn upcast_retained_journal(
    content: &str,
    input: JournalInput,
) -> Result<std::borrow::Cow<'_, [u8]>> {
    let encoded = upcast_journal(content, input)?;
    let mut value: serde_json::Value = serde_json::from_slice(&encoded)?;
    if value["version"] == 4 {
        return Ok(encoded);
    }
    for owner in value["owners"]
        .as_array_mut()
        .context("Invalid cleanup ownership journal")?
    {
        owner
            .as_object_mut()
            .context("Invalid cleanup owner")?
            .insert("purpose".into(), serde_json::json!({"kind": "purge"}));
    }
    value["version"] = serde_json::json!(4);
    let encoded = serde_json::to_vec(&value)?;
    decode(Some(std::str::from_utf8(&encoded)?))?;
    Ok(std::borrow::Cow::Owned(encoded))
}

pub(crate) fn migrate_retained_ownership(root: &Path) -> Result<()> {
    let file = open(root)?;
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let Some(content) = file.read()? else {
        // Preserve missing authority; admissions still refuse to use it.
        return Ok(());
    };
    let encoded = upcast_retained_journal(&content, JournalInput::Migration)?;
    if encoded.as_ref() != content.as_bytes() {
        super::backup_before_migration(&root.join(FILE_NAME))?;
        file.replace(&encoded)?;
        file.sync_parent()?;
    }
    Ok(())
}

fn retained_transition_recorded(
    journal: &Journal,
    storage: &Storage,
    row: &Instance,
) -> Result<bool> {
    let mut retained = journal.owners.iter().filter(|owner| {
        owner.session_id == row.id && matches!(owner.purpose, Some(OwnerPurpose::Retained { .. }))
    });
    let Some(owner) = retained.next() else {
        return Ok(false);
    };
    let Some(OwnerPurpose::Retained { instance, .. }) = owner.purpose.as_ref() else {
        unreachable!("filtered retained owner");
    };
    anyhow::ensure!(
        retained.next().is_none()
            && owner.profile == storage.profile()
            && owner.generation == row.lifecycle_generation
            && instance.trashed_at == row.trashed_at
            && instance.trash_lifecycle_id == row.trash_lifecycle_id
            && instance.lifecycle_reservation == row.lifecycle_reservation,
        super::LifecycleReservationError::Superseded
    );
    Ok(true)
}

/// The caller holds identity and lifecycle exclusion through removal of the visible row.
pub(crate) fn retain_forgotten_trash(storage: &Storage, row: &Instance) -> Result<()> {
    anyhow::ensure!(
        row.has_pending_trash() && row.failed_purge_forced().is_none(),
        "Trash transition is not an unfinished Trash-only record"
    );
    let _namespace = lock_namespace(storage, true)?;
    let file = open_for(storage)?;
    {
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        if retained_transition_recorded(&decode(file.read()?.as_deref())?, storage, row)? {
            return Ok(());
        }
    }
    let mut conversations: Vec<_> = conversation_inventory(row)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    conversations.sort_unstable();
    conversations.dedup();
    let _conversation_locks: Vec<_> = conversations
        .into_iter()
        .map(|id| lock_conversation(storage, id, true))
        .collect::<Result<_>>()?;
    let capture = PurgeCapture::new(row)?;
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let mut journal = decode(file.read()?.as_deref())?;
    if retained_transition_recorded(&journal, storage, row)? {
        return Ok(());
    }
    let mut instance = row.clone();
    instance.source_profile = storage.profile().to_owned();
    journal.owners.push(Owner {
        token: uuid::Uuid::new_v4().to_string(),
        session_id: row.id.clone(),
        profile: storage.profile().to_owned(),
        generation: row.lifecycle_generation,
        protection: capture.protection,
        runner: capture.runner,
        request: None,
        additional_protection: None,
        purpose: Some(OwnerPurpose::Retained {
            instance: Box::new(instance),
            terminal_scopes: capture.terminal_scopes,
        }),
    });
    save(&file, &journal)
}

pub(crate) fn restore_from_complete_backup(
    source: &Path,
    _transaction: &crate::daemon::lifecycle::Transaction,
    _serve: &crate::cli::serve::ServeLock,
) -> Result<()> {
    let parent = source.parent().context("Backup needs a parent directory")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let source = AnchoredDir::open(parent)?
        .bind_file(source.file_name().context("Backup needs a file name")?)?;
    let original = source
        .read_recovery_bytes()?
        .context("Purge backup is missing")?;
    let encoded = upcast_retained_journal(
        std::str::from_utf8(&original)?,
        JournalInput::CompleteBackup,
    )?;
    let app = super::get_app_dir()?;
    let _identity = super::acquire_recovery_identity_lock()?;
    let _profiles = crate::migrations::recovery::profile_locks()?;
    let file = super::LockedDataFile::lock_recovery(open(&app)?)?;
    if let Some(bytes) = file.read_recovery_bytes()? {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                let version = value.get("version").and_then(serde_json::Value::as_u64);
                anyhow::ensure!(
                    matches!(version, Some(2..=4)),
                    "Unsupported destination purge journal version; refusing replacement"
                );
            }
            anyhow::ensure!(
                upcast_retained_journal(text, JournalInput::CompleteBackup).is_err(),
                "Destination purge journal is healthy; refusing replacement"
            );
        }
        file.require_writable_existing()?;
        let quarantine = file.quarantine_recovery_bytes(&bytes)?;
        tracing::warn!(path = %quarantine.display(), "Preserved corrupt purge ownership evidence");
    }
    file.replace_and_sync(&encoded)
}

pub(crate) struct ScopeRecoveryInventory {
    raw: String,
    pub(crate) owners: Vec<(String, String)>,
}

pub(crate) fn scope_recovery_inventory() -> Result<ScopeRecoveryInventory> {
    let app = super::get_app_dir()?;
    let raw = open(&app)?.read()?.context("Pending purge ownership is missing; restore complete original authority before explicit scope recovery")?;
    let journal = decode(Some(&raw))?;
    let owners = journal
        .owners
        .into_iter()
        .map(|owner| (owner.profile, owner.session_id))
        .collect();
    Ok(ScopeRecoveryInventory { raw, owners })
}

pub(crate) struct ScopeRecoveryPlan {
    file: super::LockedDataFile,
    raw: String,
    encoded: Option<Vec<u8>>,
    path: std::path::PathBuf,
}

impl ScopeRecoveryPlan {
    pub(crate) fn check_current(&self) -> Result<()> {
        anyhow::ensure!(
            self.file.read()?.as_deref() == Some(self.raw.as_str()),
            "Purge journal changed before scope recovery; retry aoe migrate"
        );
        if self.encoded.is_some() {
            self.file.require_writable_existing()?;
        }
        Ok(())
    }

    pub(crate) fn backup(&self) -> Result<()> {
        if self.encoded.is_some() {
            super::backup_before_migration(&self.path)?;
            AnchoredDir::open(self.path.parent().context("Purge journal lacks parent")?)?.sync()?;
        }
        Ok(())
    }

    pub(crate) fn commit(self) -> Result<()> {
        self.check_current()?;
        if let Some(encoded) = self.encoded {
            self.file.replace_and_sync(&encoded)?;
        }
        Ok(())
    }
}

pub(crate) fn plan_scope_recovery(inventory: ScopeRecoveryInventory) -> Result<ScopeRecoveryPlan> {
    use crate::process::worker_registry::ScopeOwnership;
    let app = super::get_app_dir()?;
    let file = super::LockedDataFile::lock_recovery(open(&app)?)?;
    anyhow::ensure!(
        file.read()?.as_deref() == Some(inventory.raw.as_str()),
        "Purge journal changed before scope recovery; retry aoe migrate"
    );
    let mut journal = decode(Some(&inventory.raw))?;
    let mut changed = false;
    for owner in &mut journal.owners {
        let unfinished = matches!(
            &owner.runner,
            RunnerCapture::Uncaptured {
                producer_epoch: Some(_),
                ..
            }
        );
        if matches!(owner.purpose, Some(OwnerPurpose::Retained { .. })) {
            anyhow::ensure!(
                !unfinished && !matches!(owner.runner, RunnerCapture::Unresolved { .. }),
                "Protected owner {} has unresolved runtime construction; scope recovery refused",
                owner.token
            );
            continue;
        }
        let scopes = match &mut owner.runner {
            RunnerCapture::Captured { scopes, .. }
            | RunnerCapture::Uncaptured { scopes, .. }
            | RunnerCapture::CancelledBeforeBirth { scopes, .. } => Some(scopes),
            RunnerCapture::Unresolved { .. } => None,
        };
        if let Some(scopes) = scopes {
            if scopes.contains(&ScopeOwnership::Unresolved) {
                anyhow::ensure!(
                    !unfinished,
                    "Pending owner {} still has an unfinished constructor; scope recovery refused",
                    owner.token
                );
                anyhow::ensure!(crate::process::systemd::legacy_unknown_scope_absent(&owner.session_id, scopes)?,
                    "Pending owner {} has a live unknown original scope; independently identify and retire its exact original unit before retrying aoe migrate", owner.token);
                scopes.retain(|scope| *scope != ScopeOwnership::Unresolved);
                changed = true;
            }
        }
        if let Some(request) = &mut owner.request {
            if !request.instance.is_structured()
                && request
                    .instance
                    .terminal_scopes
                    .as_ref()
                    .is_none_or(|scopes| scopes.contains(&ScopeOwnership::Unresolved))
            {
                anyhow::ensure!(
                    !unfinished,
                    "Pending request {} has an unfinished constructor",
                    owner.token
                );
                let retained: Vec<_> = request
                    .instance
                    .terminal_scopes
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|scope| *scope != ScopeOwnership::Unresolved)
                    .collect();
                anyhow::ensure!(crate::process::systemd::legacy_unknown_scope_absent(&owner.session_id, &retained)?,
                    "Pending request {} has a live unknown original scope; exact original-unit retirement is required", owner.token);
                request.instance.terminal_scopes = Some(retained);
                changed = true;
            }
        }
    }
    let encoded = if changed {
        let encoded = serde_json::to_vec(&journal)?;
        decode(Some(std::str::from_utf8(&encoded)?))?;
        Some(encoded)
    } else {
        None
    };
    let plan = ScopeRecoveryPlan {
        file,
        raw: inventory.raw,
        encoded,
        path: app.join(FILE_NAME),
    };
    plan.check_current()?;
    Ok(plan)
}
impl PurgeCapture {
    pub(crate) fn new(row: &Instance) -> Result<Self> {
        let mut runner = capture_runner(&row.id);
        let terminal_scopes = if row.is_structured() {
            row.terminal_scopes.clone()
        } else {
            crate::process::systemd::resolve_unknown_scopes(&row.id, row.terminal_scopes.as_deref())
                .ok()
                .or_else(|| row.terminal_scopes.clone())
        };
        if !row.is_structured() {
            let scopes = match &mut runner {
                RunnerCapture::Captured { scopes, .. }
                | RunnerCapture::Uncaptured { scopes, .. }
                | RunnerCapture::CancelledBeforeBirth { scopes, .. } => Some(scopes),
                RunnerCapture::Unresolved { .. } => None,
            };
            if let Some(scopes) = scopes {
                for scope in terminal_scopes
                    .as_deref()
                    .unwrap_or(&[crate::process::worker_registry::ScopeOwnership::Unresolved])
                {
                    if !scopes.contains(scope) {
                        scopes.push(scope.clone());
                    }
                }
            }
        }
        let protection = CleanupProtection::new([row])?;
        protection.validate()?;
        Ok(Self {
            protection,
            runner,
            terminal_scopes,
            session_id: row.id.clone(),
        })
    }

    pub(crate) fn retire_terminal_scopes(&self) -> Result<()> {
        let scopes = self
            .terminal_scopes
            .as_deref()
            .context("Terminal scope ownership unresolved; purge resources retained")?;
        crate::process::systemd::retire_scopes(&self.session_id, scopes)
    }

    pub(crate) fn bind_retirement(
        &mut self,
        target: &crate::acp::supervisor::RetirementTarget,
    ) -> Result<()> {
        self.runner.bind_retirement(target)
    }

    pub(crate) fn ensure_captured_runner_stopped(&self) -> Result<()> {
        self.runner.ensure_stopped(Some(&self.session_id))
    }
}

impl PurgeOwner {
    #[cfg(test)]
    pub(crate) fn record(storage: &Storage, row: &Instance, capture: PurgeCapture) -> Result<Self> {
        Self::record_plan(storage, row, row.lifecycle_generation, None, None, capture)
    }

    pub(crate) fn record_plan(
        storage: &Storage,
        row: &Instance,
        generation: u64,
        request: Option<&DeletionRequest>,
        additional_protection: Option<&CleanupProtection>,
        mut capture: PurgeCapture,
    ) -> Result<Self> {
        if let RunnerCapture::Unresolved { error } = &capture.runner {
            anyhow::bail!("Runner ownership unresolved; purge refused before commit: {error}");
        }
        capture.protection.extend([row])?;
        capture.protection.validate()?;
        let file = open_for(storage)?;
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(file.read()?.as_deref())?;
        anyhow::ensure!(
            !retains_session(&journal, &row.id),
            "Retained cleanup ownership prevents purge admission"
        );
        let mut request = request.cloned();
        if let Some(request) = &mut request {
            request.session_id = row.id.clone();
            request.instance.id = row.id.clone();
            request.instance.source_profile = storage.profile().to_owned();
            request.instance.lifecycle_generation = row.lifecycle_generation;
            request.instance.lifecycle_reservation = row.lifecycle_reservation.clone();
        }
        journal.owners.push(Owner {
            token: uuid::Uuid::new_v4().to_string(),
            session_id: row.id.clone(),
            profile: storage.profile().to_owned(),
            generation,
            protection: capture.protection,
            runner: capture.runner,
            request,
            additional_protection: additional_protection.cloned(),
            purpose: Some(OwnerPurpose::Purge),
        });
        save(&file, &journal)?;
        let recorded = journal.owners.pop().expect("recorded owner");
        Ok(Self {
            file,
            token: recorded.token,
            runner: recorded.runner,
            session_id: Some(row.id.clone()),
        })
    }

    pub(super) fn recover(storage: &Storage, token: &str) -> Result<Self> {
        let file = open_for(storage)?;
        let (lock, path) = file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(file.read()?.as_deref())?;
        let index = journal
            .owners
            .iter()
            .position(|owner| owner.token == token)
            .context("Pending purge owner disappeared during recovery")?;
        let owner = &journal.owners[index];
        anyhow::ensure!(
            matches!(owner.purpose, Some(OwnerPurpose::Purge)),
            "Retained cleanup ownership cannot authorize purge recovery"
        );
        anyhow::ensure!(
            !retains_session(&journal, &owner.session_id),
            "Retained cleanup ownership prevents purge recovery"
        );
        let owner = journal.owners.swap_remove(index);
        Ok(Self {
            file,
            token: owner.token,
            runner: owner.runner,
            session_id: Some(owner.session_id),
        })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn ensure_captured_runner_stopped(&self) -> Result<()> {
        self.runner.ensure_stopped(self.session_id.as_deref())
    }

    pub(crate) fn ensure_retirement_target(
        &self,
        target: &mut crate::acp::supervisor::RetirementTarget,
    ) -> Result<()> {
        self.runner.ensure_retirement_target(target)
    }

    pub(crate) fn complete_retirement(
        &mut self,
        target: &crate::acp::supervisor::RetirementTarget,
    ) -> Result<()> {
        let runner = self.runner.completed(target)?;
        if runner == self.runner {
            return Ok(());
        }
        let (lock, path) = self.file.open_sidecar()?;
        let _guard = acquire_open_storage_flock(lock, &path)?;
        let mut journal = decode(self.file.read()?.as_deref())?;
        let index = journal
            .owners
            .iter()
            .position(|owner| owner.token == self.token)
            .context("Pending purge owner disappeared before producer settlement")?;
        anyhow::ensure!(
            !retains_session(&journal, &journal.owners[index].session_id),
            "Retained cleanup ownership prevents producer settlement"
        );
        let owner = &mut journal.owners[index];
        anyhow::ensure!(
            matches!(owner.purpose, Some(OwnerPurpose::Purge))
                && owner.runner == self.runner
                && Some(&owner.session_id) == self.session_id.as_ref(),
            "Pending purge owner changed before producer settlement"
        );
        owner.runner = runner;
        save(&self.file, &journal)?;
        self.runner = journal.owners.swap_remove(index).runner;
        Ok(())
    }

    pub(crate) fn ensure_current(&self, store: &impl super::SessionStore) -> Result<()> {
        let current = Self::recover(store.storage(), &self.token)?;
        anyhow::ensure!(
            current.session_id == self.session_id && current.runner == self.runner,
            "Pending purge owner identity changed before cleanup"
        );
        Ok(())
    }

    pub(crate) fn release(self) -> Result<()> {
        remove_token(&self.file, &self.token)
    }
}

fn remove_token(file: &ResolvedDataFile, token: &str) -> Result<()> {
    let (lock, path) = file.open_sidecar()?;
    let _guard = acquire_open_storage_flock(lock, &path)?;
    let mut journal = decode(file.read()?.as_deref())?;
    if let Some(owner) = journal.owners.iter().find(|owner| owner.token == token) {
        anyhow::ensure!(
            !retains_session(&journal, &owner.session_id),
            "Retained cleanup ownership cannot be released by purge recovery"
        );
    }
    journal.owners.retain(|owner| owner.token != token);
    save(file, &journal)
}

fn retains_session(journal: &Journal, session_id: &str) -> bool {
    journal.owners.iter().any(|owner| {
        owner.session_id == session_id
            && matches!(owner.purpose, Some(OwnerPurpose::Retained { .. }))
    })
}

pub(crate) fn protection(
    storage: &Storage,
    except: Option<&str>,
) -> Result<Vec<CleanupProtection>> {
    let journal = decode(open_for(storage)?.read()?.as_deref())?;
    let except = except.filter(|token| {
        journal
            .owners
            .iter()
            .find(|owner| owner.token == *token)
            .is_some_and(|owner| !retains_session(&journal, &owner.session_id))
    });
    Ok(journal
        .owners
        .into_iter()
        .filter(|owner| Some(owner.token.as_str()) != except)
        .flat_map(|owner| std::iter::once(owner.protection).chain(owner.additional_protection))
        .collect())
}

pub(super) fn session_ids(root: &Path) -> Result<impl Iterator<Item = String>> {
    Ok(decode_sibling(open(root)?.read()?.as_deref())?
        .owners
        .into_iter()
        .map(|owner| owner.session_id))
}

pub(super) fn recovery_plans() -> Result<Vec<RecoveryPlan>> {
    let root = crate::session::get_app_dir()?;
    let journal = decode(open(&root)?.read()?.as_deref())?;
    let mut retained = HashSet::new();
    let mut plans = Vec::new();
    for owner in journal.owners {
        match owner.purpose {
            Some(OwnerPurpose::Retained { .. }) => {
                retained.insert(owner.session_id);
            }
            Some(OwnerPurpose::Purge) => plans.push(RecoveryPlan {
                token: owner.token,
                session_id: owner.session_id,
                profile: owner.profile,
                request: owner.request,
                additional_protection: owner.additional_protection,
                protection: owner.protection,
            }),
            None => unreachable!("validated owner purpose"),
        }
    }
    plans.retain(|plan| !retained.contains(&plan.session_id));
    Ok(plans)
}

pub(super) fn release_uncommitted(plan: &RecoveryPlan) -> Result<bool> {
    let store = Storage::open_unwatched(&plan.profile)?;
    let profile_dir = store
        .sessions_path()
        .parent()
        .context("profile directory missing")?;
    // Never block here: a purge committing right now holds this lock and will
    // settle its own token, so skipping costs one pass.
    let Some(_guard) =
        try_acquire_storage_flock(profile_dir, super::storage::STORAGE_LOCK_FILENAME)?
    else {
        return Ok(false);
    };
    let Some(row) = store
        .load()?
        .into_iter()
        .find(|row| row.id == plan.session_id)
    else {
        return Ok(false);
    };
    if plan.additional_protection.is_some() {
        return Ok(false);
    }
    if !CleanupProtection::new([&row])?.covers(&plan.protection) {
        return Ok(false);
    }
    // The journal edit is a read-modify-write, so a concurrent release can
    // still be lost and resurrect the entry. That is benign: an entry whose
    // row is gone is picked up as a real recovery on the next pass, and the
    // cleanup it drives is idempotent.
    remove_token(&open_for(&store)?, &plan.token)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn legacy_ownership_migration_preserves_original_authority_and_rejects_missing_purpose(
    ) -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        let token = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let legacy = serde_json::json!({
            "version": 3,
            "future_namespace_proof": {"original": true},
            "owners": [{
                "token": token,
                "session_id": id,
                "profile": "default",
                "generation": 7,
                "protection": {
                    "paths": [{"lexical": "/retained", "traversal": null, "resolved": null}],
                    "branches": [[0, "saved-work"]]
                },
                "runner": {
                    "state": "uncaptured",
                    "scopes": [],
                    "producer_epoch": 77,
                    "producer_id": uuid::Uuid::new_v4(),
                    "prime": {"state": "unresolved"}
                },
                "future_constructor_proof": {"must_survive": [7, 77]}
            }]
        });
        let path = root.join(FILE_NAME);
        std::fs::write(&path, serde_json::to_vec(&legacy)?)?;
        assert!(CleanupOwners::load(&root).is_err());
        migrate_retained_ownership(&root)?;
        let migrated = std::fs::read(&path)?;
        let actual: serde_json::Value = serde_json::from_slice(&migrated)?;
        let mut expected = legacy;
        expected["version"] = serde_json::json!(4);
        expected["owners"][0]["purpose"] = serde_json::json!({"kind": "purge"});
        assert_eq!(actual, expected);
        assert!(decode_version(std::str::from_utf8(&migrated).ok(), 3).is_err());
        let storage = Storage::new_unwatched("default")?;
        let owner = PurgeOwner::recover(&storage, &token)?;
        assert!(owner.ensure_captured_runner_stopped().is_err());
        migrate_retained_ownership(&root)?;
        assert_eq!(std::fs::read(&path)?, migrated);

        let mut invalid = actual;
        invalid["owners"][0]
            .as_object_mut()
            .unwrap()
            .remove("purpose");
        let invalid = serde_json::to_vec(&invalid)?;
        std::fs::write(&path, &invalid)?;
        assert!(CleanupOwners::load(&root).is_err());
        assert!(migrate_retained_ownership(&root).is_err());
        assert_eq!(std::fs::read(&path)?, invalid);
        std::fs::remove_file(&path)?;
        migrate_retained_ownership(&root)?;
        assert!(CleanupOwners::load(&root).is_err());
        assert!(PurgeOwner::recover(&storage, &token).is_err());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn forget_locks_asserted_target_before_committing_and_fences_later_adoption() -> Result<()> {
        use crate::session::{ConversationBinding, ConversationProvenance, ResumeIntent};
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        initialize(&root)?;
        let storage = Storage::new_unwatched("asserted")?;
        let mut row = Instance::new("asserted target", root.to_str().unwrap());
        row.source_profile = "asserted".into();
        row.terminal_scopes = Some(Vec::new());
        let sid = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        row.resume_intent = ResumeIntent::Fork { from: sid.into() };
        row.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: None,
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        row.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Trash,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now() - chrono::Duration::hours(1),
        )?;
        row.trash();
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let journal = std::fs::read(root.join(FILE_NAME))?;
        let registry = std::fs::read(storage.sessions_path())?;
        // Hold only the target lock, not the namespace lock, to discriminate the inventory.
        let held = lock_conversation(&storage, sid, false)?;
        assert!(retain_forgotten_trash(&storage, &row).is_err());
        drop(held);
        assert_eq!(std::fs::read(root.join(FILE_NAME))?, journal);
        assert_eq!(std::fs::read(storage.sessions_path())?, registry);
        retain_forgotten_trash(&storage, &row)?;
        assert!(CleanupOwners::load(&root)?.blocks_conversation("another-row", sid, None));
        let retained = std::fs::read(root.join(FILE_NAME))?;
        let new_resources = tempfile::tempdir()?;
        let mut same_transition = row.clone();
        same_transition.project_path = new_resources.path().to_str().unwrap().into();
        retain_forgotten_trash(&storage, &same_transition)?;
        assert_eq!(std::fs::read(root.join(FILE_NAME))?, retained);
        for conflict in ["generation", "trash_identity", "timestamp", "reservation"] {
            let mut superseded = row.clone();
            match conflict {
                "generation" => {
                    superseded.lifecycle_generation += 1;
                    superseded
                        .lifecycle_reservation
                        .as_mut()
                        .unwrap()
                        .generation += 1;
                }
                "trash_identity" => {
                    superseded.trash_lifecycle_id = Some(uuid::Uuid::new_v4().to_string())
                }
                "timestamp" => {
                    superseded.trashed_at = superseded
                        .trashed_at
                        .map(|at| at - chrono::Duration::seconds(1))
                }
                _ => {
                    superseded.lifecycle_reservation.as_mut().unwrap().at -=
                        chrono::Duration::seconds(1)
                }
            }
            assert!(
                retain_forgotten_trash(&storage, &superseded)
                    .unwrap_err()
                    .is::<crate::session::LifecycleReservationError>(),
                "{conflict}"
            );
            assert_eq!(std::fs::read(root.join(FILE_NAME))?, retained);
        }
        let other_profile = Storage::new_unwatched("other-retained-profile")?;
        assert!(retain_forgotten_trash(&other_profile, &row)
            .unwrap_err()
            .is::<crate::session::LifecycleReservationError>());
        assert_eq!(std::fs::read(root.join(FILE_NAME))?, retained);
        let mut duplicate: serde_json::Value = serde_json::from_slice(&retained)?;
        let mut second = duplicate["owners"][0].clone();
        second["token"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
        duplicate["owners"].as_array_mut().unwrap().push(second);
        let duplicate = serde_json::to_vec(&duplicate)?;
        std::fs::write(root.join(FILE_NAME), &duplicate)?;
        assert!(retain_forgotten_trash(&storage, &row)
            .unwrap_err()
            .is::<crate::session::LifecycleReservationError>());
        assert_eq!(std::fs::read(root.join(FILE_NAME))?, duplicate);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn forgotten_trash_keeps_files_profile_and_runtime_authority_without_authorizing_purge(
    ) -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        initialize(&root)?;
        crate::tmux::tool_retirement::initialize(&root)?;
        for profile in ["kept", "other"] {
            crate::session::create_profile(profile)?;
        }
        crate::session::set_default_profile("other")?;
        let storage = Storage::new_unwatched("kept")?;
        let checkout = root.join("unverified-checkout");
        let main_repo = root.join("unverified-main");
        std::fs::create_dir(&checkout)?;
        std::fs::create_dir(&main_repo)?;
        let sentinel = checkout.join("kept.txt");
        std::fs::write(&sentinel, b"original files")?;
        let mut row = Instance::new("forgotten", checkout.to_str().unwrap());
        row.source_profile = "kept".into();
        row.terminal_scopes = Some(vec![
            crate::process::worker_registry::ScopeOwnership::Owned {
                unit: "aoe-test-forgotten-terminal.scope".into(),
            },
        ]);
        row.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "saved-work".into(),
            main_repo_path: main_repo.to_str().unwrap().into(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        row.trash();
        row.lifecycle_generation = 7;
        row.lifecycle_reservation = Some(crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Trash,
            generation: 7,
            at: chrono::Utc::now() - chrono::Duration::hours(1),
        });
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let older_checkout = root.join("older-owned-checkout");
        std::fs::create_dir(&older_checkout)?;
        let mut prior_row = row.clone();
        prior_row.project_path = older_checkout.to_str().unwrap().into();
        let prior = PurgeOwner::record(&storage, &prior_row, PurgeCapture::new(&prior_row)?)?;
        let prior_token = prior.token().to_owned();
        let obstruction = root.join(".aoe-trash");
        std::fs::write(&obstruction, b"user-owned obstruction")?;
        let safe_capture = PurgeCapture::new(&row)?;
        let worker_path = crate::process::worker_registry::record_path(&row.id)?;
        std::fs::create_dir_all(worker_path.parent().unwrap())?;
        let worker_bytes = b"{\"incomplete\":true}";
        std::fs::write(&worker_path, worker_bytes)?;
        retain_forgotten_trash(&storage, &row)?;
        assert!(
            PurgeOwner::record(&storage, &row, safe_capture).is_err(),
            "retained ownership must also fence ordinary purge admission"
        );
        assert!(prior.release().is_err());
        storage.update(|rows, _| {
            rows.clear();
            Ok(())
        })?;
        let journal_bytes = std::fs::read(root.join(FILE_NAME))?;
        let journal = decode(std::str::from_utf8(&journal_bytes).ok())?;
        let token = &journal
            .owners
            .iter()
            .find(|owner| matches!(owner.purpose, Some(OwnerPurpose::Retained { .. })))
            .unwrap()
            .token;
        assert!(PurgeOwner::recover(&storage, &prior_token).is_err());
        assert!(PurgeOwner::recover(&storage, token).is_err());
        assert!(recovery_plans()?.is_empty());
        assert!(protection(&storage, Some(token))?
            .iter()
            .any(|owner| owner.references_path(&checkout)));
        assert!(protection(&storage, Some(&prior_token))?
            .iter()
            .any(|owner| owner.references_path(&older_checkout)));
        let owners = CleanupOwners::load(&root)?;
        assert!(owners
            .protections()
            .any(|owner| owner.references_path(&checkout)));
        assert!(owners
            .protections()
            .any(|owner| owner.references_branch(&main_repo, "saved-work")));
        let holding = super::super::trash::trash_holding_path(&checkout, &row.id).unwrap();
        assert!(owners
            .protections()
            .any(|owner| owner.references_path(&holding)));
        for error in [
            crate::session::delete_profile("kept").unwrap_err(),
            crate::session::rename_profile("kept", "renamed").unwrap_err(),
        ] {
            assert!(
                error.downcast_ref::<ProfileHasPendingPurges>().is_some(),
                "{error:#}"
            );
        }
        assert!(plan_scope_recovery(scope_recovery_inventory()?).is_err());
        assert_eq!(std::fs::read(root.join(FILE_NAME))?, journal_bytes);
        assert_eq!(std::fs::read(&worker_path)?, worker_bytes);
        assert_eq!(std::fs::read(&sentinel)?, b"original files");
        assert_eq!(std::fs::read(&obstruction)?, b"user-owned obstruction");
        assert!(CleanupOwners::load(&root)?.retains_session(&row.id));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn retained_alias_purge_owner_fences_its_physical_profile_catalogue() -> Result<()> {
        for leaf in ["sessions.json", "groups.json"] {
            for delete in [false, true] {
                let _home = crate::session::test_support::isolate_app_dir();
                let root = crate::session::get_app_dir()?;
                initialize(&root)?;
                crate::tmux::tool_retirement::initialize(&root)?;
                for name in ["source", "keep", "distinct"] {
                    crate::session::create_profile(name)?;
                }
                crate::session::set_default_profile("source")?;
                let source = Storage::new_unwatched("source")?;
                source.update(|_, _| Ok(()))?;
                std::fs::write(source.sessions_path().with_file_name("groups.json"), b"[]")?;
                let alias_dir = root.join("profiles/alias");
                // Owner admission requires a real profile directory; only its
                // user-managed registry file aliases the source binding.
                std::fs::create_dir(&alias_dir)?;
                if leaf == "groups.json" {
                    std::fs::write(alias_dir.join("sessions.json"), b"[]")?;
                }
                std::os::unix::fs::symlink(
                    source.sessions_path().with_file_name(leaf),
                    alias_dir.join(leaf),
                )?;
                let alias = Storage::open_unwatched("alias")?;
                assert_eq!(alias.profile(), "alias");
                let checkout = root.join("owned-checkout");
                std::fs::create_dir(&checkout)?;
                let mut row = Instance::new("alias retained owner", checkout.to_str().unwrap());
                row.source_profile = "alias".into();
                row.view = crate::session::View::Terminal;
                row.terminal_scopes = Some(vec![]);
                alias.update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })?;
                if leaf == "sessions.json" {
                    assert_eq!(
                        source.load()?[0].id,
                        row.id,
                        "owner was admitted through the file alias"
                    );
                } else {
                    assert!(
                        source.load()?.is_empty(),
                        "groups alias must retain a distinct session registry"
                    );
                    assert_eq!(alias.load()?[0].id, row.id);
                }
                let request = DeletionRequest {
                    session_id: row.id.clone(),
                    instance: row.clone(),
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: true,
                };
                let owner = PurgeOwner::record_plan(
                    &alias,
                    &row,
                    row.lifecycle_generation,
                    Some(&request),
                    None,
                    PurgeCapture::new(&row)?,
                )?;
                let token = owner.token().to_owned();
                drop(owner);
                alias.update(|rows, _| {
                    rows.clear();
                    Ok(())
                })?;
                let launch = serde_json::json!({
                    "schema": 4, "pid": 4242, "instance_id": "catalogue-policy-fixture",
                    "profile": "source", "core_only": false, "host": "127.0.0.1", "port": 7777,
                    "auth_mode": "none", "has_passphrase": false, "behind_proxy": false,
                    "read_only": false, "remote": false, "tunnel_name": null, "tunnel_url": null,
                    "no_tailscale": true,
                });
                let _: crate::cli::serve::ServeLaunch = serde_json::from_value(launch.clone())?;
                let launch_bytes = serde_json::to_vec_pretty(&launch)?;
                for name in ["serve.launch", "serve.rollback.launch"] {
                    std::fs::write(root.join(name), &launch_bytes)?;
                }
                // A retained owner in another physical registry is not a blanket lock.
                crate::session::rename_profile("distinct", "distinct-renamed")?;
                crate::session::delete_profile("distinct-renamed")?;
                let config_before = std::fs::read(root.join("config.toml"))?;
                let journal_before = std::fs::read(root.join(FILE_NAME))?;
                let mutation = if delete {
                    crate::session::delete_profile("source")
                } else {
                    crate::session::rename_profile("source", "renamed")
                };
                let error =
                    mutation.expect_err("catalogue must retain the alias-owned recovery target");
                assert!(error.is::<ProfileHasPendingPurges>(), "{error:?}");
                assert!(root.join("profiles/source").is_dir());
                assert!(alias_dir.is_dir());
                assert!(!root.join("profiles/renamed").exists());
                assert_eq!(std::fs::read(root.join("config.toml"))?, config_before);
                assert_eq!(std::fs::read(root.join(FILE_NAME))?, journal_before);
                for name in ["serve.launch", "serve.rollback.launch"] {
                    assert_eq!(std::fs::read(root.join(name))?, launch_bytes);
                }
                let recovered = crate::session::deletion::recover_committed_purge(
                    &mut std::collections::HashSet::new(),
                )?
                .expect("the unchanged logical alias must still recover its committed owner");
                assert_eq!(recovered.owner_token(), token);
                let result = recovered.finish(|_, _| Ok(()));
                assert!(result.success, "{result:?}");
                assert!(recovery_plans()?.is_empty());
                if delete {
                    crate::session::delete_profile("source")?;
                } else {
                    crate::session::rename_profile("source", "renamed")?;
                }
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn old_pr_45_capture_never_borrows_birth_or_prime_authority_from_a_replacement() -> Result<()> {
        use crate::process::worker_registry::{self, WorkerRecord};
        let home = tempfile::TempDir::with_prefix_in("aoe-frozen-", "/tmp")?;
        let _home = super::super::test_support::isolate_app_dir_at(home.path());
        let app = super::super::get_app_dir()?;
        initialize(&app)?;
        let storage = Storage::new_unwatched("default")?;
        let mut row = Instance::new("retained owner", home.path().to_str().unwrap());
        row.source_profile = "default".into();
        let record = WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            worker_registry::socket_path_for(&row.id)?,
            "agent".into(),
            "agent".into(),
            home.path().into(),
            None,
            vec![],
            vec![],
            None,
            Some("default".into()),
        )
        .with_generation(7);
        row.lifecycle_generation = 7;
        row.terminal_scopes = Some(Vec::new());
        worker_registry::save(&record)?;
        let owner = PurgeOwner::record(&storage, &row, PurgeCapture::new(&row)?)?;
        let token = owner.token().to_owned();
        drop(owner);
        let journal_path = app.join(FILE_NAME);
        let mut legacy: serde_json::Value = serde_json::from_slice(&std::fs::read(&journal_path)?)?;
        legacy["version"] = serde_json::json!(2);
        legacy["owners"][0]
            .as_object_mut()
            .unwrap()
            .remove("purpose");
        let runner = legacy["owners"][0]["runner"].as_object_mut().unwrap();
        runner.remove("prime");
        runner.remove("birth_fingerprint");
        let legacy_bytes = serde_json::to_vec(&legacy)?;
        std::fs::write(&journal_path, &legacy_bytes)?;
        assert!(PurgeOwner::recover(&storage, &token).is_err());
        let mut replacement = record.with_generation(8);
        replacement.agent_key = "prime-agent".into();
        replacement.prime_runtime = Some(crate::process::prime::PrimeRuntime::new(
            "/bin/false".into(),
            "/tmp/store".into(),
            None,
        )?);
        worker_registry::save(&replacement)?;
        let record_path = worker_registry::record_path(&row.id)?;
        let replacement_bytes = std::fs::read(&record_path)?;
        std::fs::write(app.join(".schema_version"), "45")?;
        crate::migrations::run_migrations()?;
        let migrated_bytes = std::fs::read(&journal_path)?;
        let migrated: serde_json::Value = serde_json::from_slice(&migrated_bytes)?;
        assert_eq!(migrated["owners"][0]["runner"]["generation"], 7);
        assert_eq!(
            migrated["owners"][0]["runner"]["prime"],
            serde_json::json!({"state":"unresolved"})
        );
        assert!(migrated["owners"][0]["runner"]["birth_fingerprint"].is_null());
        let owner = PurgeOwner::recover(&storage, &token)?;
        let mut target = crate::acp::supervisor::RetirementTarget {
            lease: None,
            identity: Some(crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                &replacement,
            )),
            scopes: vec![replacement.scope.clone()],
            prime: PrimeOwnership::from_record(&replacement),
            journalled: false,
        };
        let error = owner.ensure_retirement_target(&mut target).unwrap_err();
        assert!(error.to_string().contains("authority remains unresolved"));
        let error = owner.ensure_captured_runner_stopped().unwrap_err();
        assert!(error
            .to_string()
            .contains("Native runner ownership is unresolved"));
        assert_eq!(std::fs::read(&record_path)?, replacement_bytes);
        assert_eq!(std::fs::read(&journal_path)?, migrated_bytes);
        assert!(super::super::migration_backups(&journal_path)?
            .iter()
            .any(|(_, path)| std::fs::read(path).is_ok_and(|bytes| bytes == legacy_bytes)));
        crate::migrations::run_migrations_announced(None)?;
        assert_eq!(std::fs::read(journal_path)?, migrated_bytes);
        assert_eq!(std::fs::read(record_path)?, replacement_bytes);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn an_absent_journal_is_no_owners_but_a_corrupt_one_is_still_refused() -> Result<()> {
        // A namespace predating pending-purge ownership has no journal at all, and it holds no
        // pending owners. The store reclaim reads this as a sibling, so an
        // absent file must not fail the pass and orphan every store.
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        assert!(session_ids(root.path())?.next().is_none());

        // A journal that exists but does not parse is corruption, not absence:
        // staying fail-closed is the point.
        std::fs::write(root.path().join(FILE_NAME), b"{")?;
        assert!(session_ids(root.path()).is_err());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn frozen_owners_survive_alias_loss_and_reject_incomplete_journals() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        initialize(root.path())?;
        let storage = Storage::new_for_test_path("source", root.path().join("sessions.json"));
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        std::fs::create_dir(&real)?;
        std::os::unix::fs::symlink(&real, &alias)?;
        let mut row = Instance::new("owner", alias.join("checkout").to_str().unwrap());
        row.worktree_info = Some(super::super::WorktreeInfo {
            branch: "work".into(),
            main_repo_path: alias.to_string_lossy().into_owned(),
            managed_by_aoe: false,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let owner = PurgeOwner::record(&storage, &row, PurgeCapture::new(&row)?)?;
        let peer_path = root.path().join("peer");
        let peer_row = Instance::new("peer", peer_path.to_str().unwrap());
        let peer = PurgeOwner::record(&storage, &peer_row, PurgeCapture::new(&peer_row)?)?;
        initialize(root.path())?;
        std::fs::remove_file(&alias)?;
        let frozen = protection(&storage, None)?;
        assert!(frozen
            .iter()
            .any(|entry| entry.references_path(&real.join("checkout"))));
        assert!(frozen
            .iter()
            .any(|entry| entry.references_branch(&real, "work")));
        let path = root.path().join(FILE_NAME);
        let saved = std::fs::read(&path)?;
        let mut invalid_index: serde_json::Value = serde_json::from_slice(&saved)?;
        invalid_index["owners"][0]["protection"]["branches"] = serde_json::json!([[999, "work"]]);
        for corrupt in [b"{".to_vec(), serde_json::to_vec(&invalid_index)?] {
            std::fs::write(&path, corrupt)?;
            assert!(protection(&storage, None).is_err());
            assert!(PurgeOwner::record(&storage, &row, PurgeCapture::new(&row)?).is_err());
        }
        std::fs::remove_file(&path)?;
        assert!(
            protection(&storage, None).is_err(),
            "a missing journal cannot prove absent ownership"
        );
        std::fs::write(&path, saved)?;
        owner.release()?;
        let remaining = protection(&storage, None)?;
        assert!(!remaining.iter().any(|entry| entry.references_path(&real)));
        assert!(remaining
            .iter()
            .any(|entry| entry.references_path(&peer_path)));
        peer.release()?;
        assert!(!protection(&storage, None)?
            .iter()
            .any(|entry| entry.references_path(&peer_path)));
        Ok(())
    }

    /// A process-group check alone cannot distinguish a reaped child from a
    /// replacement that reused its pid. The frozen pid+generation must still
    /// match the strict registry, and ambiguous registry reads retain data.
    #[test]
    fn runner_cleanup_checks_reaped_replacement_and_unreadable_registry() -> Result<()> {
        fn record(pid: u32, generation: u64) -> crate::process::worker_registry::WorkerRecord {
            let mut record = crate::process::worker_registry::WorkerRecord::new(
                "session".into(),
                pid,
                std::path::PathBuf::from("unused.sock"),
                String::new(),
                String::new(),
                std::path::PathBuf::new(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .with_generation(generation);
            record.runner_identity = Some(crate::process::ProcessBirthIdentity {
                boot_id: "test-boot".into(),
                start_id: format!("process-{pid}"),
            });
            record
        }

        let captured = RunnerCapture::Captured {
            pid: 41,
            generation: 7,
            birth_fingerprint: record(41, 7)
                .runner_identity
                .as_ref()
                .map(|identity| identity.fingerprint()),
            prime: PrimeOwnership::NotPrime,
            scopes: vec![crate::process::worker_registry::ScopeOwnership::Unscoped],
        };
        captured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(41, 7))),
                |_| false,
                |_| Ok(None),
            )
            .expect("an exact, reaped registry identity releases cleanup");
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Ok(Some(record(std::process::id(), 8))),
                |_| true,
                |_| Ok(None),
            )
            .expect_err("a live replacement must retain purge resources");
        assert!(error.to_string().contains("identity was replaced"));
        let error = captured
            .ensure_stopped_with(
                Some("session"),
                |_| Err(anyhow::anyhow!("permission denied")),
                |_| false,
                |_| Ok(None),
            )
            .expect_err("an unreadable registry is ambiguous");
        assert!(error.to_string().contains("unreadable"));
        captured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| true, |_| Ok(None))
            .expect_err("a missing registry cannot prove a live captured process exited");
        captured
            .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(Some(99)))
            .expect_err("an active socket blocks a missing registry");

        RunnerCapture::Unresolved {
            error: "worker record unreadable".into(),
        }
        .ensure_stopped(None)
        .expect_err("unresolved ownership blocks cleanup");
        RunnerCapture::Uncaptured {
            scopes: Vec::new(),
            producer_epoch: None,
            producer_id: None,
            prime: PrimeOwnership::NotPrime,
        }
        .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(None))
        .expect("a strict registry absence and inactive sockets prove no runner");
        RunnerCapture::Uncaptured {
            scopes: Vec::new(),
            producer_epoch: None,
            producer_id: None,
            prime: PrimeOwnership::NotPrime,
        }
        .ensure_stopped_with(Some("session"), |_| Ok(None), |_| false, |_| Ok(Some(42)))
        .expect_err("an active socket prevents an absent capture from authorizing cleanup");
        RunnerCapture::Uncaptured {
            scopes: Vec::new(),
            producer_epoch: None,
            producer_id: None,
            prime: PrimeOwnership::NotPrime,
        }
        .ensure_stopped_with(
            Some("session"),
            |_| Ok(Some(record(42, 1))),
            |_| false,
            |_| Ok(None),
        )
        .expect_err("a runner published after capture is unresolved");
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn malformed_registry_is_unresolved_at_capture() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let row = Instance::new(
            "malformed-registry",
            root.path().join("checkout").to_str().unwrap(),
        );
        let record = crate::process::worker_registry::record_path(&row.id)?;
        std::fs::write(record, b"{")?;

        let capture = PurgeCapture::new(&row)?;

        assert!(capture.ensure_captured_runner_stopped().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn active_socket_without_registry_is_unresolved() -> Result<()> {
        // Under /tmp, not $TMPDIR: macOS paths exceed the 104-byte sun_path limit.
        let root = tempfile::TempDir::with_prefix_in("purge-owner-socket", "/tmp")?;
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let row = Instance::new(
            "socket-without-registry",
            root.path().join("checkout").to_str().unwrap(),
        );
        let socket = crate::process::worker_registry::socket_path_for(&row.id)?;
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;

        let capture = PurgeCapture::new(&row)?;

        assert!(capture.ensure_captured_runner_stopped().is_err());
        Ok(())
    }
    #[test]
    #[serial_test::serial]
    fn complete_restore_preserves_removed_row_request_and_path_protection() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        initialize(&app)?;
        let store = Storage::new_unwatched("default")?;
        let checkout = home.path().join("retained-checkout");
        std::fs::create_dir(&checkout)?;
        std::fs::write(checkout.join("sentinel"), b"retained")?;
        let mut row = Instance::new("original", checkout.to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Terminal;
        row.terminal_scopes = Some(Vec::new());
        store.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let request = DeletionRequest {
            session_id: row.id.clone(),
            instance: row.clone(),
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: true,
        };
        let owner = PurgeOwner::record_plan(
            &store,
            &row,
            row.lifecycle_generation,
            Some(&request),
            None,
            PurgeCapture::new(&row)?,
        )?;
        let token = owner.token().to_owned();
        drop(owner);
        store.update(|rows, _| {
            rows.clear();
            Ok(())
        })?;
        let target = app.join(FILE_NAME);
        let original = std::fs::read(&target)?;
        let backup = home.path().join("complete-purge-backup.json");
        std::fs::write(&backup, &original)?;
        std::fs::remove_file(&target)?;
        assert!(protection(&store, None).is_err());
        assert_eq!(std::fs::read(checkout.join("sentinel"))?, b"retained");
        let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
            std::time::Duration::from_secs(1),
        )?;
        let serve = crate::cli::serve::try_lock_serve()?.unwrap();
        let corrupt_bytes = b"retained corrupt purge evidence";
        for corrupt in [false, true] {
            if corrupt {
                std::fs::write(&target, corrupt_bytes)?;
            } else if target.exists() {
                std::fs::remove_file(&target)?;
            }
            for missing in ["request", "additional_protection"] {
                let mut incomplete: serde_json::Value = serde_json::from_slice(&original)?;
                incomplete["owners"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove(missing);
                let incomplete_path = home.path().join("incomplete-purge-backup.json");
                std::fs::write(&incomplete_path, serde_json::to_vec(&incomplete)?)?;
                assert!(
                    crate::cli::serve::with_inactive_serve_state(
                        &transaction,
                        Some(&serve),
                        |serve| {
                            restore_from_complete_backup(&incomplete_path, &transaction, serve)
                        },
                    )
                    .is_err(),
                    "missing {missing}"
                );
                if corrupt {
                    assert_eq!(std::fs::read(&target)?, corrupt_bytes);
                } else {
                    assert!(!target.exists());
                }
                assert_eq!(std::fs::read(checkout.join("sentinel"))?, b"retained");
            }
            crate::cli::serve::with_inactive_serve_state(&transaction, Some(&serve), |serve| {
                restore_from_complete_backup(&backup, &transaction, serve)
            })?;
            assert_eq!(std::fs::read(&target)?, original);
            let plans = recovery_plans()?;
            assert_eq!(plans.len(), 1);
            assert_eq!(plans[0].token, token);
            let request = plans[0].request.as_ref().unwrap();
            assert_eq!(request.instance.project_path, row.project_path);
            assert!(request.keep_scratch);
            assert_eq!(protection(&store, None)?.len(), 1);
            assert!(crate::cli::serve::with_inactive_serve_state(
                &transaction,
                Some(&serve),
                |serve| { restore_from_complete_backup(&backup, &transaction, serve) }
            )
            .is_err());
            assert_eq!(std::fs::read(&target)?, original);
            assert_eq!(std::fs::read(checkout.join("sentinel"))?, b"retained");
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn historical_backup_upcast_preserves_unknown_authority_and_refuses_invalid_identity(
    ) -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        initialize(&app)?;
        let store = Storage::new_unwatched("default")?;
        let mut row = Instance::new("legacy", home.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Terminal;
        row.terminal_scopes = Some(Vec::new());
        let owner = PurgeOwner::record(&store, &row, PurgeCapture::new(&row)?)?;
        let token = owner.token().to_owned();
        drop(owner);
        let mut old: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join(FILE_NAME))?)?;
        old["version"] = serde_json::json!(2);
        old["owners"][0].as_object_mut().unwrap().remove("purpose");
        old["owners"][0].as_object_mut().unwrap().remove("request");
        old["owners"][0]
            .as_object_mut()
            .unwrap()
            .remove("additional_protection");
        let runner = old["owners"][0]["runner"].as_object_mut().unwrap();
        runner.remove("prime");
        runner.remove("scopes");
        runner.remove("producer_id");
        let old_json = serde_json::to_string(&old)?;
        let converted = upcast_journal(&old_json, JournalInput::CompleteBackup)?;
        let value: serde_json::Value = serde_json::from_slice(&converted)?;
        assert_eq!(value["owners"][0]["token"], token);
        assert_eq!(
            value["owners"][0]["protection"],
            old["owners"][0]["protection"]
        );
        assert_eq!(value["owners"][0]["runner"]["prime"]["state"], "unresolved");
        assert_eq!(
            value["owners"][0]["runner"]["scopes"],
            serde_json::json!([{"state":"unresolved"}])
        );
        assert_eq!(
            upcast_journal(
                std::str::from_utf8(&converted)?,
                JournalInput::CompleteBackup
            )?,
            converted
        );
        let mut future = old.clone();
        future["version"] = serde_json::json!(u32::MAX);
        assert!(upcast_journal(
            &serde_json::to_string(&future)?,
            JournalInput::CompleteBackup
        )
        .is_err());
        old["owners"][0]["session_id"] = serde_json::json!("../foreign");
        assert!(
            upcast_journal(&serde_json::to_string(&old)?, JournalInput::CompleteBackup).is_err()
        );
        Ok(())
    }
}

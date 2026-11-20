use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use super::{BroadcastSink, Lease, LifecycleTable, Supervisor, SupervisorError};
use crate::acp::state::{
    NativeEventContext, NativeForkRejection, NativeObservationScope, NativeObservationWait,
};
use crate::session::{ConversationState, Instance};

pub(super) struct ForkSeed {
    scope: NativeObservationScope,
    conversation: ConversationState,
    acp_session_id: Option<String>,
    parent: String,
    import_pending: Option<bool>,
    agent_name: Option<String>,
    agent_provider: Option<String>,
    project_path: String,
    sandboxed: bool,
}

impl ForkSeed {
    fn matches_selection(&self, row: &Instance) -> bool {
        self.conversation.matches_checkpoint(row)
            && self.acp_session_id == row.acp_session_id
            && self.agent_name == row.agent_name
            && self.agent_provider == row.agent_provider
            && self.project_path == row.project_path
            && self.sandboxed == row.is_sandboxed()
    }

    fn matches_seed(&self, row: &Instance) -> bool {
        self.matches_selection(row)
            && row.fork_pending.as_deref() == Some(self.parent.as_str())
            && row.import_pending == self.import_pending
    }

    fn matches_postimage(&self, row: &Instance) -> bool {
        self.matches_selection(row) && row.fork_pending.is_none() && row.import_pending.is_none()
    }
}

#[derive(Default)]
pub(super) struct ForkCustody {
    seed: Option<Arc<ForkSeed>>,
    receipt: Option<Arc<QualifiedForkRejection>>,
}

impl ForkCustody {
    pub(super) fn prepare(&mut self, seed: Arc<ForkSeed>) {
        self.seed = Some(seed);
    }

    pub(super) fn receive(
        &mut self,
        lease: &Lease,
        proof: &Arc<NativeForkRejection>,
    ) -> Option<Arc<QualifiedForkRejection>> {
        let seed = self.seed.as_ref()?;
        if proof.owner.birth_fingerprint.is_none()
            || lease.spawned_runner() != Some(proof.owner)
            || proof.parent != seed.parent
        {
            return None;
        }
        if let Some(receipt) = &self.receipt {
            return Arc::ptr_eq(&receipt.proof, proof).then(|| receipt.clone());
        }
        let receipt = Arc::new(QualifiedForkRejection {
            seed: seed.clone(),
            proof: proof.clone(),
            epoch: lease.epoch(),
            producer: lease.producer_id(),
            settlement: AtomicU8::new(0),
            settled: tokio::sync::Notify::new(),
        });
        self.receipt = Some(receipt.clone());
        Some(receipt)
    }

    pub(super) fn receipt(&self) -> Option<Arc<QualifiedForkRejection>> {
        self.receipt.clone()
    }
}

pub(crate) struct QualifiedForkRejection {
    seed: Arc<ForkSeed>,
    proof: Arc<NativeForkRejection>,
    epoch: u64,
    producer: uuid::Uuid,
    settlement: AtomicU8,
    settled: tokio::sync::Notify,
}

impl std::fmt::Debug for QualifiedForkRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QualifiedForkRejection")
            .finish_non_exhaustive()
    }
}

impl QualifiedForkRejection {
    fn finish(&self, committed: bool) {
        let _ = self.settlement.compare_exchange(
            0,
            if committed { 1 } else { 2 },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.settled.notify_waiters();
    }

    pub(super) fn committed(&self) -> bool {
        self.settlement.load(Ordering::Acquire) == 1
    }

    pub(super) async fn wait(&self) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.settlement.load(Ordering::Acquire) != 0 {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn matches_postimage(&self, row: &Instance) -> bool {
        self.seed.matches_postimage(row)
    }

    pub(crate) fn context(&self) -> NativeEventContext {
        self.seed.scope.context
    }
}

#[derive(Debug, thiserror::Error)]
#[error("original fork selection was superseded")]
struct ForkSuperseded;

pub(super) async fn prepare_fork_seed(
    lease: &Lease,
    parent: Option<&str>,
) -> Result<(), SupervisorError> {
    let Some(parent) = parent else {
        return Ok(());
    };
    let Some(admission) = lease.admission() else {
        return Ok(());
    };
    let Some(scope) = admission.native_scope.clone() else {
        return Err(SupervisorError::NativeStoreUnavailable);
    };
    let id = lease.session_id().to_owned();
    let parent = parent.to_owned();
    let seed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let store = admission.store;
        let _native = store.storage().acquire_instance_lifecycle_lock(&id)?;
        let row = store
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            NativeEventContext::capture(&row, store.storage())? == scope.context
                && row.fork_pending.as_deref() == Some(parent.as_str()),
            ForkSuperseded
        );
        Ok(Arc::new(ForkSeed {
            scope,
            conversation: row.conversation_state(),
            acp_session_id: row.acp_session_id.clone(),
            parent,
            import_pending: row.import_pending,
            agent_name: row.agent_name.clone(),
            agent_provider: row.agent_provider.clone(),
            project_path: row.project_path.clone(),
            sandboxed: row.is_sandboxed(),
        }))
    })
    .await
    .map_err(|_| SupervisorError::NativeStoreUnavailable)?
    .map_err(|_| SupervisorError::SpawnCancelled(lease.session_id().to_owned()))?;
    lease.retirement().prepare_fork_seed(seed);
    Ok(())
}

fn try_checkpoint(
    lifecycle: &Arc<std::sync::Mutex<LifecycleTable>>,
    lease: &Lease,
    receipt: &QualifiedForkRejection,
) -> anyhow::Result<Option<NativeObservationWait>> {
    if receipt.settlement.load(Ordering::Acquire) != 0 {
        return Ok(None);
    }
    let current = match lifecycle.try_lock() {
        Ok(current) => current,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Ok(Some(NativeObservationWait::lifecycle(
                receipt.seed.scope.context,
                lifecycle.clone(),
            )));
        }
        Err(std::sync::TryLockError::Poisoned(_)) => anyhow::bail!("lifecycle unavailable"),
    };
    let original = current.current_lease(lease.session_id()).as_ref() == Some(lease)
        && lease.epoch() == receipt.epoch
        && lease.producer_id() == receipt.producer;
    drop(current);
    if !original {
        receipt.finish(false);
        return Ok(None);
    }
    let admission = lease
        .admission()
        .ok_or(crate::session::NativeStoreUnavailable)?;
    let store = admission.store;
    let Some(_native) = store
        .storage()
        .try_acquire_instance_lifecycle_lock(lease.session_id())?
    else {
        return Ok(Some(NativeObservationWait::native(&receipt.seed.scope)));
    };
    let result = store.commit_metadata_checkpoint(&mut |rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == lease.session_id())
            .ok_or(ForkSuperseded)?;
        let same_context =
            NativeEventContext::capture(row, store.storage())? == receipt.seed.scope.context;
        let original = receipt.seed.matches_seed(row);
        let applied = receipt.seed.matches_postimage(row);
        let current = lifecycle.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => anyhow::anyhow!(super::NativeObservationBusy),
            std::sync::TryLockError::Poisoned(_) => anyhow::anyhow!("lifecycle unavailable"),
        })?;
        anyhow::ensure!(
            same_context
                && (original || applied)
                && current.current_lease(lease.session_id()).as_ref() == Some(lease),
            ForkSuperseded
        );
        if original {
            row.fork_pending = None;
            row.import_pending = None;
        }
        Ok(())
    });
    match result {
        Ok(()) => receipt.finish(true),
        Err(error) if error.is::<ForkSuperseded>() => receipt.finish(false),
        Err(error) if error.is::<super::NativeObservationBusy>() => {
            return Ok(Some(NativeObservationWait::lifecycle(
                receipt.seed.scope.context,
                lifecycle.clone(),
            )));
        }
        Err(error) => {
            // Do not reuse launch-only presentation admission for this retained
            // metadata receipt. A strict read of the same bound physical store
            // can prove supersession; unreadable/applied-unadopted state cannot.
            let current = store
                .storage()
                .verify_bound_profile()
                .and_then(|()| store.storage().load_complete_with_groups());
            if let Ok((rows, _)) = current {
                let superseded = match rows.iter().find(|row| row.id == lease.session_id()) {
                    None => true,
                    Some(row) => {
                        NativeEventContext::capture(row, store.storage()).is_ok_and(|context| {
                            context != receipt.seed.scope.context
                                || (!receipt.seed.matches_seed(row)
                                    && !receipt.seed.matches_postimage(row))
                                || row.ensure_startable().is_err()
                        })
                    }
                };
                if superseded {
                    receipt.finish(false);
                    return Ok(None);
                }
            }
            return Err(error);
        }
    }
    Ok(None)
}

pub(super) async fn checkpoint(
    lifecycle: &Arc<std::sync::Mutex<LifecycleTable>>,
    lease: &Lease,
    receipt: &Arc<QualifiedForkRejection>,
) {
    loop {
        let table = lifecycle.clone();
        let original = lease.clone();
        let retained = receipt.clone();
        match tokio::task::spawn_blocking(move || try_checkpoint(&table, &original, &retained))
            .await
        {
            Ok(Ok(Some(wait))) => {
                if !wait.settle().await {
                    return;
                }
            }
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                tracing::warn!(target: "acp.fork", session = %lease.session_id(), %error,
                    "native fork rejection checkpoint remains unconfirmed");
                return;
            }
            Err(error) => {
                tracing::warn!(target: "acp.fork", session = %lease.session_id(), %error,
                    "native fork rejection checkpoint task failed");
                return;
            }
        }
    }
}

impl<S: BroadcastSink> Supervisor<S> {
    pub(crate) fn fork_rejection_superseded(
        &self,
        id: &str,
        receipt: &QualifiedForkRejection,
    ) -> bool {
        self.lifecycle
            .try_lock()
            .ok()
            .and_then(|table| table.current_lease(id))
            .is_some_and(|lease| {
                lease.epoch() != receipt.epoch || lease.producer_id() != receipt.producer
            })
    }

    pub(crate) async fn settle_fork_rejections(&self) {
        let pending: Vec<_> = {
            let workers = self.workers.lock().await;
            workers
                .values()
                .filter_map(|handle| {
                    handle
                        .lease
                        .retirement()
                        .fork_rejection()
                        .filter(|receipt| receipt.settlement.load(Ordering::Acquire) == 0)
                        .map(|receipt| (handle.lease.clone(), receipt))
                })
                .collect()
        };
        for (lease, receipt) in pending {
            let lifecycle = self.lifecycle.clone();
            let _ =
                tokio::task::spawn_blocking(move || try_checkpoint(&lifecycle, &lease, &receipt))
                    .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::supervisor::test_support::HeldAttachRunner;
    use crate::acp::supervisor::{LaunchAdmission, ResumeKind};
    use crate::session::{
        ConversationBinding, ConversationProvenance, ExecutionBinding, SessionStore,
    };

    /// Producer correlation has its own runner test. This boundary starts with
    /// an already captured private receipt, then lets its real original child
    /// exit before the canonical checkpoint is allowed to consume it.
    #[tokio::test]
    #[serial_test::serial]
    async fn captured_rejection_after_child_exit_consumes_only_the_full_original_seed() {
        #[derive(Clone, Copy, Debug)]
        enum Change {
            None,
            Title,
            Applied,
            AppliedOtherRoute,
            Parent,
            Import,
            Session,
            Route,
            Generation,
            Producer,
            PhysicalStore,
        }
        for change in [
            Change::None,
            Change::Title,
            Change::Applied,
            Change::AppliedOtherRoute,
            Change::Parent,
            Change::Import,
            Change::Session,
            Change::Route,
            Change::Generation,
            Change::Producer,
            Change::PhysicalStore,
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::TempDir::new().unwrap();
            let mut row = Instance::new("original fork", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            row.fork_pending = Some("original-parent".into());
            row.import_pending = Some(true);
            row.resume_binding = Some(ConversationBinding {
                session_id: "original-parent".into(),
                execution: Some(ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![project.path().join("native")],
                    configuration: Vec::new(),
                    cwd: project.path().into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: Some(true),
                }),
                provenance: ConversationProvenance::Observed,
                transcript_path: None,
            });
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let (store, ()) = crate::server::session_store::NativeSessionStore::open_for_launch(
                state.clone(),
                &row,
                (),
            )
            .await
            .unwrap();
            let scope = NativeObservationScope::capture(&row, store.storage()).unwrap();
            let lifecycle = Arc::new(std::sync::Mutex::new(LifecycleTable::new(1)));
            let lease = lifecycle
                .lock()
                .unwrap()
                .admit(&row.id, ResumeKind::SpawnFirst)
                .unwrap();
            lease.set_admission(Some(LaunchAdmission {
                store: store.clone(),
                generation: row.lifecycle_generation,
                namespace: None,
                native_scope: Some(scope),
            }));
            let mut original = HeldAttachRunner::ready(
                &row.id,
                project.path(),
                lease.epoch(),
                Some("default"),
                None,
            )
            .await;
            assert!(lease.record_runner(original.identity));
            prepare_fork_seed(&lease, Some("original-parent"))
                .await
                .unwrap();
            lifecycle
                .lock()
                .unwrap()
                .install(&lease, Some(original.identity))
                .unwrap();
            let mut weak = original.identity;
            weak.birth_fingerprint = None;
            for proof in [
                NativeForkRejection {
                    owner: weak,
                    parent: "original-parent".into(),
                },
                NativeForkRejection {
                    owner: original.identity,
                    parent: "another-parent".into(),
                },
            ] {
                assert!(lease
                    .retirement()
                    .receive_fork_rejection(&lease, &Arc::new(proof))
                    .is_none());
            }
            assert!(lease.retirement().fork_rejection().is_none());
            let receipt = lease
                .retirement()
                .receive_fork_rejection(
                    &lease,
                    &Arc::new(NativeForkRejection {
                        owner: original.identity,
                        parent: "original-parent".into(),
                    }),
                )
                .unwrap();
            // Capture before physical exit. Do not manufacture a replacement
            // producer's authority from native generation or matching SID.
            original.exit_peer();
            let mut expected = row.clone();
            match change {
                Change::None | Change::Producer | Change::PhysicalStore => {}
                Change::Title => expected.title = "renamed fork".into(),
                Change::Applied | Change::AppliedOtherRoute => {
                    expected.fork_pending = None;
                    expected.import_pending = None;
                    if matches!(change, Change::AppliedOtherRoute) {
                        expected
                            .resume_binding
                            .as_mut()
                            .unwrap()
                            .execution
                            .as_mut()
                            .unwrap()
                            .exported_default_store = Some(false);
                    }
                }
                Change::Parent => expected.fork_pending = Some("replacement-parent".into()),
                Change::Import => expected.import_pending = Some(false),
                Change::Session => expected.acp_session_id = Some("replacement-native".into()),
                Change::Route => {
                    expected
                        .resume_binding
                        .as_mut()
                        .unwrap()
                        .execution
                        .as_mut()
                        .unwrap()
                        .exported_default_store = Some(false);
                    assert_eq!(
                        expected.resume_binding, row.resume_binding,
                        "ordinary binding equality intentionally omits route provenance"
                    );
                }
                Change::Generation => expected.lifecycle_generation += 1,
            }
            let update = expected.clone();
            let id = row.id.clone();
            let updating_state = state.clone();
            tokio::task::spawn_blocking(move || {
                let canonical = crate::server::session_store::NativeSessionStore::open(
                    updating_state,
                    "default",
                    None,
                )?;
                canonical.commit(&mut |rows, _| {
                    *rows.iter_mut().find(|value| value.id == id).unwrap() = update.clone();
                    Ok(())
                })
            })
            .await
            .unwrap()
            .unwrap();
            if matches!(change, Change::Producer) {
                let mut table = lifecycle.lock().unwrap();
                assert!(table.release_running(&lease));
                let replacement = table.admit(&row.id, ResumeKind::SpawnFirst).unwrap();
                assert_ne!(replacement, lease);
                assert_eq!(expected.lifecycle_generation, row.lifecycle_generation);
            }
            let mut retired_profile = None;
            if matches!(change, Change::PhysicalStore) {
                let path = store
                    .storage()
                    .sessions_path()
                    .parent()
                    .unwrap()
                    .to_path_buf();
                let retired = path.with_file_name("retired-original-profile");
                let before = std::fs::read(store.storage().sessions_path()).unwrap();
                std::fs::rename(&path, &retired).unwrap();
                std::fs::create_dir(&path).unwrap();
                let replacement = crate::session::Storage::new_unwatched("default").unwrap();
                replacement
                    .update(|rows, _| {
                        *rows = vec![expected.clone()];
                        Ok(())
                    })
                    .unwrap();
                retired_profile = Some((retired.join("sessions.json"), before));
            }
            checkpoint(&lifecycle, &lease, &receipt).await;
            let accepted = matches!(change, Change::None | Change::Title | Change::Applied);
            assert_eq!(receipt.committed(), accepted, "{change:?}");
            if accepted {
                expected.fork_pending = None;
                expected.import_pending = None;
            }
            let persisted = crate::session::Storage::open_reference("default")
                .unwrap()
                .load_complete_with_groups()
                .unwrap()
                .0;
            let persisted = persisted.iter().find(|value| value.id == row.id).unwrap();
            assert_eq!(persisted.fork_pending, expected.fork_pending, "{change:?}");
            assert_eq!(
                persisted.import_pending, expected.import_pending,
                "{change:?}"
            );
            assert_eq!(
                persisted.acp_session_id, expected.acp_session_id,
                "{change:?}"
            );
            assert_eq!(
                persisted
                    .resume_binding
                    .as_ref()
                    .unwrap()
                    .execution
                    .as_ref()
                    .unwrap()
                    .exported_default_store,
                expected
                    .resume_binding
                    .as_ref()
                    .unwrap()
                    .execution
                    .as_ref()
                    .unwrap()
                    .exported_default_store,
                "{change:?}"
            );
            if let Some((path, before)) = retired_profile {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    before,
                    "never mutate the retired physical profile"
                );
            } else {
                let memory = state.instances.read().await;
                let memory = memory.iter().find(|value| value.id == row.id).unwrap();
                assert_eq!(memory.fork_pending, expected.fork_pending, "{change:?}");
                assert_eq!(memory.import_pending, expected.import_pending, "{change:?}");
            }
            state.shutdown.cancel();
        }
    }
}

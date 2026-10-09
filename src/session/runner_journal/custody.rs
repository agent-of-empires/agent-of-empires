use super::{bootstrap, LaunchOrigin, NativeLaunchPhase, RunnerLaunch};
use crate::acp::runner_lifecycle::{ExecutionAdmission, ExecutionJob, RunnerIdentity};
use crate::process::OriginalRootDeathObservation;
use crate::session::storage::StorageFlock;
use anyhow::{Context, Result};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

trait RetainedCustody: Send + Sync {
    fn nonce(&self) -> Uuid;
}

static LIVE: Mutex<Vec<Arc<dyn RetainedCustody>>> = Mutex::new(Vec::new());

pub(super) trait OriginalChild: Send + 'static {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl OriginalChild for std::process::Child {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.try_wait()
    }
}

impl OriginalChild for tokio::process::Child {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.try_wait()
    }
}

pub(super) struct OriginalLaunchCustody<C: OriginalChild> {
    nonce: Uuid,
    pub(super) admission: ExecutionAdmission,
    state: Mutex<LaunchState<C>>,
}

pub(super) struct LaunchState<C> {
    pub(super) original: Arc<LaunchOrigin>,
    pub(super) child: Option<C>,
    pub(super) observation: Option<(RunnerIdentity, OriginalRootDeathObservation)>,
    pub(super) identity: Option<RunnerIdentity>,
    pub(super) fences: Option<[StorageFlock; 3]>,
    pub(super) channel: Option<UnixStream>,
    pub(super) watchdog: Option<bootstrap::ParentNatalGuard>,
    pub(super) spawn_attempted: bool,
    pub(super) authorization_attempted: bool,
    channels_closed: bool,
    terminal: Option<OriginalTerminal>,
    terminal_observed: bool,
    receipt: Option<Arc<OriginalNoTargetReceipt>>,
    job: Option<ExecutionJob>,
}

enum OriginalTerminal {
    Wait(std::process::ExitStatus),
    Kernel(RunnerIdentity, OriginalRootDeathObservation),
    NeverSpawned,
}

pub(crate) struct OriginalNoTargetReceipt {
    original: Arc<LaunchOrigin>,
    launch: RunnerLaunch,
    identity: Option<RunnerIdentity>,
    terminal: OriginalTerminal,
}

impl OriginalNoTargetReceipt {
    pub(crate) fn original(&self) -> &Arc<LaunchOrigin> {
        &self.original
    }

    pub(crate) fn launch(&self) -> &RunnerLaunch {
        &self.launch
    }

    pub(crate) fn identity(&self) -> Option<RunnerIdentity> {
        self.identity
    }

    pub(crate) fn never_spawned(&self) -> bool {
        matches!(self.terminal, OriginalTerminal::NeverSpawned)
    }
}

impl<C: OriginalChild> RetainedCustody for OriginalLaunchCustody<C> {
    fn nonce(&self) -> Uuid {
        self.nonce
    }
}

impl<C: OriginalChild> OriginalLaunchCustody<C> {
    pub(super) fn register(
        nonce: Uuid,
        original: Arc<LaunchOrigin>,
        admission: ExecutionAdmission,
        job: ExecutionJob,
    ) -> Result<Arc<Self>> {
        anyhow::ensure!(
            job.belongs_to(&admission),
            "constructor replaced its original execution job"
        );
        let custody = Arc::new(Self {
            nonce,
            admission,
            state: Mutex::new(LaunchState {
                original,
                child: None,
                observation: None,
                identity: None,
                fences: None,
                channel: None,
                watchdog: None,
                spawn_attempted: false,
                authorization_attempted: false,
                channels_closed: false,
                terminal: None,
                terminal_observed: false,
                receipt: None,
                job: Some(job),
            }),
        });
        let mut live = LIVE.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            live.iter().all(|existing| existing.nonce() != nonce),
            "constructor nonce already has original custody"
        );
        live.push(custody.clone());
        Ok(custody)
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, LaunchState<C>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn observe_original_terminal(&self) -> Result<bool> {
        let mut state = self.lock();
        if state.terminal_observed {
            return Ok(true);
        }
        if !state.spawn_attempted {
            state.terminal = Some(OriginalTerminal::NeverSpawned);
            state.terminal_observed = true;
            return Ok(true);
        }
        let wait = state
            .child
            .as_mut()
            .context("original child is absent")?
            .try_wait();
        if let Ok(Some(status)) = wait {
            state.terminal = Some(OriginalTerminal::Wait(status));
            state.terminal_observed = true;
            return Ok(true);
        }
        if let Some((_, observation)) = state.observation.as_ref() {
            if observation.exited()? {
                let (identity, observation) = state.observation.take().expect("observed original");
                state.terminal = Some(OriginalTerminal::Kernel(identity, observation));
                state.terminal_observed = true;
                return Ok(true);
            }
            return Ok(false);
        }
        wait?;
        Ok(false)
    }

    pub(super) fn close_parent_channels(&self) {
        let (channel, watchdog) = {
            let mut state = self.lock();
            (state.channel.take(), state.watchdog.take())
        };
        if let Some(channel) = channel {
            let _ = channel.shutdown(std::net::Shutdown::Both);
            drop(channel);
        }
        drop(watchdog);
        self.lock().channels_closed = true;
    }

    pub(super) fn no_target_receipt(
        &self,
        launch: RunnerLaunch,
    ) -> Result<Arc<OriginalNoTargetReceipt>> {
        anyhow::ensure!(
            self.observe_original_terminal()?,
            "original root remains live"
        );
        let mut state = self.lock();
        anyhow::ensure!(
            state.channels_closed
                && !state.authorization_attempted
                && matches!(
                    launch.phase,
                    NativeLaunchPhase::Unresolved {
                        may_authorize: false
                    }
                )
                && launch.nonce == *self.nonce.as_bytes()
                && state.original.births.contains(&launch),
            "original no-target custody is incomplete or authorization was attempted"
        );
        if let Some(receipt) = &state.receipt {
            anyhow::ensure!(
                receipt.launch() == &launch && Arc::ptr_eq(receipt.original(), &state.original),
                "original no-target receipt was rebound"
            );
            return Ok(receipt.clone());
        }
        let terminal = state
            .terminal
            .as_ref()
            .context("original terminal witness is absent")?;
        match terminal {
            OriginalTerminal::NeverSpawned => anyhow::ensure!(
                state.identity.is_none() && !state.spawn_attempted,
                "never-spawned evidence changed"
            ),
            OriginalTerminal::Wait(status) => {
                let _ = status;
                anyhow::ensure!(
                    state
                        .identity
                        .is_none_or(|identity| identity.birth_is_complete()),
                    "original waited root birth is incomplete"
                );
            }
            OriginalTerminal::Kernel(identity, observation) => anyhow::ensure!(
                Some(*identity) == state.identity && observation.exited()?,
                "original prebound terminal observation changed"
            ),
        }
        let receipt = Arc::new(OriginalNoTargetReceipt {
            original: state.original.clone(),
            launch,
            identity: state.identity,
            terminal: state.terminal.take().expect("validated original terminal"),
        });
        state.receipt = Some(receipt.clone());
        Ok(receipt)
    }

    pub(super) fn release_failed_fences(&self, refusal: &RunnerLaunch) -> Result<()> {
        let mut state = self.lock();
        anyhow::ensure!(
            state.terminal_observed
                && state.channels_closed
                && state.original.births.contains(refusal)
                && matches!(refusal.phase, NativeLaunchPhase::Unresolved { .. }),
            "original fences lack root-terminal, closed-channel and durable refusal evidence"
        );
        if let Some(fences) = state.fences.as_ref() {
            for fence in fences {
                fence.unlock_held()?;
            }
        }
        drop(state.fences.take());
        Ok(())
    }

    pub(super) fn release_acknowledged(
        &self,
        ack: &super::OriginalNoTargetRemovalAck,
    ) -> Result<()> {
        let retired = self
            .admission
            .preparation_retirement()
            .context("original preparation has no retirement acknowledgement channel")?;
        anyhow::ensure!(
            *retired.borrow() == Some(true),
            "original job cannot retire before its preparation durable ACK"
        );
        let job = {
            let mut state = self.lock();
            anyhow::ensure!(
                Arc::ptr_eq(&state.original, ack.original())
                    && state.channels_closed
                    && state.fences.is_none(),
                "no-target removal ACK belongs to another original custody"
            );
            state.original = ack.derived().clone();
            drop(state.receipt.take());
            state.job.take()
        };
        drop(job);
        LIVE.lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|entry| entry.nonce() != self.nonce);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) struct ManagedChild {
    pub(super) custody: Arc<OriginalLaunchCustody<std::process::Child>>,
    pub(super) pid: u32,
}

#[cfg(test)]
impl ManagedChild {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.custody
            .lock()
            .child
            .as_mut()
            .expect("original child")
            .try_wait()
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        let mut state = self.custody.lock();
        if state
            .child
            .as_mut()
            .expect("original child")
            .try_wait()?
            .is_some()
        {
            return Ok(());
        }
        state.child.as_mut().expect("original child").kill()
    }

    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.custody
            .lock()
            .child
            .as_mut()
            .expect("original child")
            .wait()
    }
}

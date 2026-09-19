//! Canonical runtime subscription. Disconnects retain the last displayed state.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::TryRecvError,
        Arc,
    },
};

use crate::daemon::{
    CreateSessionBody, CreationProgress, EnsureToolBody, MutationReceipt, RuntimeEvent,
    StartSessionBody, TerminalSize, TerminalTarget,
};
use crate::daemon::{RuntimeConnection, RuntimeCursor, RuntimeSnapshot, SessionMutation};
use crate::session::AuxiliaryTarget;

#[derive(Debug, Clone)]
pub(crate) enum SessionFeedResult {
    Snapshot(Arc<RuntimeSnapshot>),
    /// No daemon answered. The reason is for the transition log only.
    Unavailable(String),
}

/// Where the sidebar's daemon-owned state currently comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarSource {
    Connecting,
    /// Disconnected; the last canonical state remains displayed.
    Disconnected,
    /// A reachable daemon publishing canonical runtime snapshots.
    Daemon,
}

const COMMAND_CAPACITY: usize = 32;

enum SessionRequest {
    Mutation(SessionMutation),
    EnsureAuxiliary {
        target: AuxiliaryTarget,
        size: Option<TerminalSize>,
    },
    /// Prepare the session's own agent pane.
    EnsureAgent {
        size: Option<TerminalSize>,
    },
    /// Daemon-owned creation; the daemon assigns the session id.
    Create(Box<crate::daemon::CreateSessionBody>),
    CancelCreation,
}

enum CommandReply {
    Mutation(RuntimeCursor),
    Terminal(MutationReceipt<TerminalTarget>),
    Restart(MutationReceipt<crate::daemon::RestartOutcome>),
    Created(Box<MutationReceipt<crate::daemon::SessionResponse>>),
}

impl CommandReply {
    fn cursor(&self) -> &RuntimeCursor {
        match self {
            Self::Mutation(cursor) => cursor,
            Self::Terminal(receipt) => &receipt.cursor,
            Self::Restart(receipt) => &receipt.cursor,
            Self::Created(receipt) => &receipt.cursor,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NativeLease {
    grant: Arc<AtomicU64>,
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

impl NativeLease {
    pub(crate) fn is_valid(&self) -> bool {
        !self.cancelled.load(Ordering::SeqCst)
            && self.grant.load(Ordering::SeqCst) == self.generation
    }

    pub(crate) fn revoke(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

pub(crate) struct NativePreparation {
    pub lease: NativeLease,
    pub result: tokio::sync::oneshot::Receiver<Result<String, String>>,
}

struct SessionCommand {
    id: String,
    request: SessionRequest,
    lease: u64,
    native: Option<NativeLease>,
    result: tokio::sync::oneshot::Sender<Result<CommandReply, String>>,
}

struct PendingTerminal {
    /// `None` means the session's own agent pane, which the snapshot reports
    /// through `agent_pane` rather than the auxiliary list.
    target: Option<AuxiliaryTarget>,
    cancelled: Arc<AtomicBool>,
    result: tokio::sync::oneshot::Sender<Result<String, String>>,
}

struct PendingCommand {
    grant: Arc<AtomicU64>,
    lease: u64,
    result: tokio::sync::oneshot::Receiver<Result<CommandReply, String>>,
    reply: Option<CommandReply>,
    terminal: Option<PendingTerminal>,
    marks_unread: bool,
}

impl PendingCommand {
    fn fail(&mut self, id: &str, message: String, errors: &mut Vec<SessionCommandError>) {
        if let Some(terminal) = self.terminal.take() {
            let _ = terminal.result.send(Err(message));
        } else {
            errors.push(SessionCommandError {
                id: id.into(),
                message,
                marks_unread: self.marks_unread,
            });
        }
    }
}

pub(crate) struct SessionCommandError {
    pub id: String,
    pub message: String,
    pub marks_unread: bool,
}

/// One in-flight daemon creation, correlated with the caller's own row by
/// `token` because the session id does not exist until the daemon commits.
struct PendingCreation {
    grant: Arc<AtomicU64>,
    lease: u64,
    result: tokio::sync::oneshot::Receiver<Result<CommandReply, String>>,
}

fn set_grant(grant: &AtomicU64, allowed: bool) {
    let _ = grant.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        ((value & 1 == 1) != allowed).then(|| value + 1)
    });
}

pub struct SessionFeed {
    sender: tokio::sync::watch::Sender<Option<SessionFeedResult>>,
    receiver: tokio::sync::watch::Receiver<Option<SessionFeedResult>>,
    /// Advisory creation progress, on its own channel so a busy hook cannot
    /// overwrite an unapplied canonical snapshot.
    progress: tokio::sync::watch::Sender<Option<Vec<CreationProgress>>>,
    progress_receiver: tokio::sync::watch::Receiver<Option<Vec<CreationProgress>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    grant: Arc<AtomicU64>,
    native_grant: Arc<AtomicU64>,
    commands: Option<tokio::sync::mpsc::Sender<SessionCommand>>,
    pending: HashMap<String, PendingCommand>,
    pending_creations: HashMap<String, PendingCreation>,
    applied: Option<Arc<RuntimeSnapshot>>,
}

impl SessionFeed {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (progress, progress_receiver) = tokio::sync::watch::channel(None);
        Self {
            sender,
            receiver,
            progress,
            progress_receiver,
            task: None,
            grant: Arc::new(AtomicU64::new(0)),
            native_grant: Arc::new(AtomicU64::new(0)),
            commands: None,
            pending: HashMap::new(),
            pending_creations: HashMap::new(),
            applied: None,
        }
    }

    pub fn connect(&mut self, profile: String) {
        set_grant(&self.grant, false);
        set_grant(&self.native_grant, false);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        self.sender = sender;
        self.receiver = receiver;
        self.applied = None;
        let grant = Arc::new(AtomicU64::new(0));
        self.grant = grant.clone();
        let native_grant = Arc::new(AtomicU64::new(0));
        self.native_grant = native_grant.clone();
        let (commands, mut requests) =
            tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(commands);
        let (progress, progress_receiver) = tokio::sync::watch::channel(None);
        self.progress = progress.clone();
        self.progress_receiver = progress_receiver;
        let sender = self.sender.clone();
        self.task = Some(tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let endpoint = crate::acp::client::daemon_manager::ensure_daemon(&profile).await?;
                let client = endpoint.daemon_client()?;
                let mut connection = RuntimeConnection::connect(
                    &endpoint, (!profile.is_empty()).then_some(profile.as_str()),
                ).await?;
                let epoch = connection.info().epoch.clone();
                set_grant(&grant, connection.mutations_allowed());
                set_grant(&native_grant, connection.native_interaction_allowed());
                sender.send_replace(Some(SessionFeedResult::Snapshot(connection.snapshot().clone())));
                progress.send_replace(Some(connection.creation_progress().to_vec()));
                let read = async {
                    loop {
                        match connection.next_event().await {
                            Ok(RuntimeEvent::Snapshot(snapshot)) => {
                                set_grant(&grant, connection.mutations_allowed());
                                set_grant(&native_grant, connection.native_interaction_allowed());
                                sender.send_replace(Some(SessionFeedResult::Snapshot(snapshot)));
                            }
                            Ok(RuntimeEvent::Progress(progresses)) => {
                                progress.send_replace(Some(progresses));
                            }
                            Err(error) => {
                                set_grant(&grant, false);
                                set_grant(&native_grant, false);
                                progress.send_replace(Some(Vec::new()));
                                sender.send_replace(Some(SessionFeedResult::Unavailable(error.to_string())));
                                break;
                            }
                        }
                    }
                };
                let write = async {
                    while let Some(request) = requests.recv().await {
                        let allowed = request.native.as_ref().map_or_else(
                            || grant.load(Ordering::SeqCst) == request.lease,
                            NativeLease::is_valid,
                        );
                        let result = if !allowed {
                            Err("Request cancelled before submission: runtime permission was revoked".into())
                        } else {
                            match request.request {
                                SessionRequest::Mutation(SessionMutation::Restart(body)) => client
                                    .restart_session(&request.id, &body, &epoch)
                                    .await.map(CommandReply::Restart),
                                SessionRequest::Mutation(mutation) => client
                                    .mutate_session(&request.id, &mutation, &epoch)
                                    .await.map(CommandReply::Mutation),
                                SessionRequest::EnsureAuxiliary { target, size } => {
                                    let receipt = match target {
                                        AuxiliaryTarget::Host { index } => client.ensure_terminal(
                                            &request.id, index, &StartSessionBody { size }, &epoch,
                                        ).await,
                                        AuxiliaryTarget::Container { index } => client.ensure_container_terminal(
                                            &request.id, index, &StartSessionBody { size }, &epoch,
                                        ).await,
                                        AuxiliaryTarget::Tool { tool_name } => client.ensure_tool(
                                            &request.id, &EnsureToolBody { tool_name, size }, &epoch,
                                        ).await,
                                    };
                                    receipt.map(CommandReply::Terminal)
                                }
                                SessionRequest::EnsureAgent { size } => client
                                    .ensure_agent(
                                        &request.id,
                                        &StartSessionBody { size },
                                        &epoch,
                                    )
                                    .await
                                    .map(CommandReply::Terminal),
                                SessionRequest::Create(body) => client
                                    .create_session(&body, &epoch)
                                    .await
                                    .map(|receipt| CommandReply::Created(Box::new(receipt))),
                                SessionRequest::CancelCreation => client
                                    .cancel_creation(&request.id, &epoch)
                                    .await
                                    .map(CommandReply::Mutation),
                            }.map_err(|error| match error {
                                crate::daemon::DaemonClientError::Status { code: Some(crate::daemon::ApiErrorCode::ResumeFailed), .. } =>
                                    "Resume failed; the conversation is preserved for explicit retry".into(),
                                error => error.to_string(),
                            })
                        };
                        let _ = request.result.send(result);
                    }
                };
                tokio::join!(read, write);
                Ok(())
            }.await;
            set_grant(&grant, false);
            set_grant(&native_grant, false);
            if let Err(error) = result {
                sender.send_replace(Some(SessionFeedResult::Unavailable(error.to_string())));
            }
        }));
    }

    pub(crate) fn mutations_available(&self) -> bool {
        self.grant.load(Ordering::SeqCst) & 1 == 1
            && self
                .commands
                .as_ref()
                .is_some_and(|commands| !commands.is_closed())
    }

    pub(crate) fn native_interaction_available(&self) -> bool {
        self.native_grant.load(Ordering::SeqCst) & 1 == 1
            && self
                .commands
                .as_ref()
                .is_some_and(|commands| !commands.is_closed())
    }

    pub(crate) fn can_submit(&self, id: &str) -> bool {
        self.mutations_available()
            && self.pending.len() < COMMAND_CAPACITY
            && !self.pending.contains_key(id)
    }

    pub(crate) fn submit(&mut self, id: String, mutation: SessionMutation) -> anyhow::Result<()> {
        self.enqueue(id, SessionRequest::Mutation(mutation), None, None)
    }

    /// Latest creation progress, once. `None` means nothing new arrived, so an
    /// idle tick costs no allocation.
    pub(crate) fn drain_progress(&mut self) -> Option<Vec<CreationProgress>> {
        if !self.progress_receiver.has_changed().unwrap_or(false) {
            return None;
        }
        self.progress_receiver.borrow_and_update().clone()
    }

    /// Submit a daemon-owned creation. `token` identifies the caller's own
    /// pending row until the daemon publishes the committed session.
    pub(crate) fn create_session(
        &mut self,
        token: String,
        body: CreateSessionBody,
    ) -> anyhow::Result<()> {
        let grant = self.grant.clone();
        let lease = grant.load(Ordering::SeqCst);
        anyhow::ensure!(
            lease & 1 == 1,
            "Runtime disconnected, read-only or unhealthy; no change submitted"
        );
        anyhow::ensure!(
            !self.pending_creations.contains_key(&token),
            "This creation is already pending"
        );
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Runtime disconnected"))?;
        let (result, receiver) = tokio::sync::oneshot::channel();
        commands
            .try_send(SessionCommand {
                id: token.clone(),
                request: SessionRequest::Create(Box::new(body)),
                native: None,
                lease,
                result,
            })
            .map_err(|_| {
                anyhow::anyhow!("Runtime command queue unavailable; no change submitted")
            })?;
        self.pending_creations.insert(
            token,
            PendingCreation {
                grant,
                lease,
                result: receiver,
            },
        );
        Ok(())
    }

    /// Request deferred cancellation of a daemon creation. The acknowledgement
    /// is the daemon's, not a promise that the rollback has finished.
    pub(crate) fn cancel_creation(&mut self, id: String) -> anyhow::Result<()> {
        self.enqueue(id, SessionRequest::CancelCreation, None, None)
    }

    /// Settled creations, each with the daemon's receipt or its refusal.
    pub(crate) fn drain_creation_results(
        &mut self,
    ) -> Vec<(
        String,
        Result<MutationReceipt<crate::daemon::SessionResponse>, String>,
    )> {
        let mut settled = Vec::new();
        self.pending_creations.retain(|token, pending| {
            if pending.grant.load(Ordering::SeqCst) != pending.lease {
                settled.push((
                    token.clone(),
                    Err("runtime permission revoked; creation outcome unknown".into()),
                ));
                return false;
            }
            match pending.result.try_recv() {
                Ok(Ok(CommandReply::Created(receipt))) => {
                    settled.push((token.clone(), Ok(*receipt)));
                    false
                }
                Ok(Ok(_)) => {
                    settled.push((token.clone(), Err("unexpected creation reply".into())));
                    false
                }
                Ok(Err(error)) => {
                    settled.push((token.clone(), Err(error)));
                    false
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    settled.push((
                        token.clone(),
                        Err("runtime request interrupted; creation outcome unknown".into()),
                    ));
                    false
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
            }
        });
        settled
    }

    /// Prepare the session's own agent pane. Readiness is confirmed by the
    /// applied snapshot's agent observation, like an auxiliary's.
    pub(crate) fn ensure_agent(
        &mut self,
        id: String,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<NativePreparation> {
        let generation = self.native_grant.load(Ordering::SeqCst);
        anyhow::ensure!(generation & 1 == 1, "Native interaction is unavailable");
        // No row-membership check here: the applied snapshot can trail by a frame
        // while the row is already known locally, and the daemon validates the id
        // itself. Only the epoch must match, so the receipt can be fenced.
        anyhow::ensure!(
            self.applied.as_ref().is_some_and(|applied| {
                matches!(&*self.sender.borrow(), Some(SessionFeedResult::Snapshot(latest))
                    if latest.cursor.epoch == applied.cursor.epoch)
            }),
            "Runtime snapshot is not current"
        );
        let lease = NativeLease {
            grant: self.native_grant.clone(),
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let size = size.and_then(|(cols, rows)| {
            Some(TerminalSize {
                cols: std::num::NonZeroU16::new(cols)?,
                rows: std::num::NonZeroU16::new(rows)?,
            })
        });
        let (ready, result) = tokio::sync::oneshot::channel();
        self.enqueue(
            id,
            SessionRequest::EnsureAgent { size },
            Some(lease.clone()),
            Some(PendingTerminal {
                target: None,
                cancelled: lease.cancelled.clone(),
                result: ready,
            }),
        )?;
        Ok(NativePreparation { lease, result })
    }

    pub(crate) fn restart_agent(
        &mut self,
        id: String,
        body: crate::daemon::RestartSessionBody,
    ) -> anyhow::Result<NativePreparation> {
        let generation = self.native_grant.load(Ordering::SeqCst);
        anyhow::ensure!(generation & 1 == 1, "Native interaction is unavailable");
        anyhow::ensure!(
            self.applied.as_ref().is_some_and(|applied| {
                matches!(&*self.sender.borrow(), Some(SessionFeedResult::Snapshot(latest))
                if latest.cursor.epoch == applied.cursor.epoch)
            }),
            "Runtime snapshot is not current"
        );
        let lease = NativeLease {
            grant: self.native_grant.clone(),
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (ready, result) = tokio::sync::oneshot::channel();
        self.enqueue(
            id,
            SessionRequest::Mutation(SessionMutation::Restart(body)),
            Some(lease.clone()),
            Some(PendingTerminal {
                target: None,
                cancelled: lease.cancelled.clone(),
                result: ready,
            }),
        )?;
        Ok(NativePreparation { lease, result })
    }

    pub(crate) fn has_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    pub(crate) fn ensure_auxiliary(
        &mut self,
        id: String,
        target: AuxiliaryTarget,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<NativePreparation> {
        let generation = self.native_grant.load(Ordering::SeqCst);
        anyhow::ensure!(generation & 1 == 1, "Native interaction is unavailable");
        // Only the epoch is checked at admission: the applied snapshot can trail
        // by a frame while the row is already known locally, and the daemon
        // validates the id itself. Readiness still requires a snapshot that
        // reports this target alive, which is what fences the receipt.
        anyhow::ensure!(
            self.applied.as_ref().is_some_and(|applied| {
                matches!(&*self.sender.borrow(), Some(SessionFeedResult::Snapshot(latest))
                    if latest.cursor.epoch == applied.cursor.epoch)
            }),
            "Runtime snapshot is not current"
        );
        let lease = NativeLease {
            grant: self.native_grant.clone(),
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let size = size.and_then(|(cols, rows)| {
            Some(TerminalSize {
                cols: std::num::NonZeroU16::new(cols)?,
                rows: std::num::NonZeroU16::new(rows)?,
            })
        });
        let (ready, result) = tokio::sync::oneshot::channel();
        self.enqueue(
            id,
            SessionRequest::EnsureAuxiliary {
                target: target.clone(),
                size,
            },
            Some(lease.clone()),
            Some(PendingTerminal {
                target: Some(target),
                cancelled: lease.cancelled.clone(),
                result: ready,
            }),
        )?;
        Ok(NativePreparation { lease, result })
    }

    fn enqueue(
        &mut self,
        id: String,
        request: SessionRequest,
        native: Option<NativeLease>,
        terminal: Option<PendingTerminal>,
    ) -> anyhow::Result<()> {
        let (grant, lease) = match &native {
            Some(native) => (native.grant.clone(), native.generation),
            None => (self.grant.clone(), self.grant.load(Ordering::SeqCst)),
        };
        anyhow::ensure!(
            lease & 1 == 1 && grant.load(Ordering::SeqCst) == lease,
            "Runtime disconnected, read-only or unhealthy; no change submitted"
        );
        anyhow::ensure!(
            !self.pending.contains_key(&id),
            "This session already has a pending runtime change"
        );
        anyhow::ensure!(
            self.pending.len() < COMMAND_CAPACITY,
            "Runtime command queue is full"
        );
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Runtime disconnected"))?;
        let (result, receiver) = tokio::sync::oneshot::channel();
        let marks_unread = matches!(&request, SessionRequest::Mutation(SessionMutation::Unread(body)) if body.unread);
        commands
            .try_send(SessionCommand {
                id: id.clone(),
                request,
                native,
                lease,
                result,
            })
            .map_err(|_| {
                anyhow::anyhow!("Runtime command queue unavailable; no change submitted")
            })?;
        self.pending.insert(
            id,
            PendingCommand {
                grant,
                lease,
                result: receiver,
                reply: None,
                terminal,
                marks_unread,
            },
        );
        Ok(())
    }

    pub(crate) fn applied_session(&self, id: &str) -> Option<&crate::daemon::SessionResponse> {
        self.applied
            .as_ref()?
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
    }

    pub(crate) fn mark_snapshot_applied(&mut self, snapshot: Arc<RuntimeSnapshot>) -> bool {
        let permissions_changed = self.applied.as_ref().is_none_or(|previous| {
            previous.contents.health != snapshot.contents.health
                || previous.contents.capabilities != snapshot.contents.capabilities
        });
        self.applied = Some(snapshot);
        permissions_changed
    }

    pub(crate) fn drain_command_errors(&mut self) -> Vec<SessionCommandError> {
        let snapshot = self.applied.as_ref();
        let mut errors = Vec::new();
        self.pending.retain(|id, pending| {
            if pending.reply.is_none() {
                match pending.result.try_recv() {
                    Ok(Ok(reply)) => pending.reply = Some(reply),
                    Ok(Err(error)) => {
                        pending.fail(id, error, &mut errors);
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        pending.fail(
                            id,
                            "runtime request interrupted; outcome unknown".into(),
                            &mut errors,
                        );
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return true,
                }
            }
            if pending.grant.load(Ordering::SeqCst) != pending.lease
                || pending
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.cancelled.load(Ordering::SeqCst))
            {
                pending.fail(
                    id,
                    "runtime permission revoked; change not confirmed".into(),
                    &mut errors,
                );
                return false;
            }
            if let (Some(reply), Some(snapshot)) = (&pending.reply, snapshot) {
                let cursor = reply.cursor();
                if cursor.epoch == snapshot.cursor.epoch
                    && cursor.revision <= snapshot.cursor.revision
                {
                    if let Some(terminal) = pending.terminal.take() {
                        let PendingTerminal {
                            target,
                            result: terminal,
                            ..
                        } = terminal;
                        let alive = snapshot
                            .contents
                            .sessions
                            .iter()
                            .find(|row| row.id == *id)
                            .is_some_and(|row| {
                                match &target {
                                    // The agent observation lags the ensure it
                                    // answers (the sampler publishes on its own
                                    // cadence), so the receipt is the proof: it
                                    // names the pane the daemon just ensured.
                                    None => match reply {
                                        CommandReply::Restart(receipt) => row.lifecycle_generation == receipt.outcome.lifecycle_generation
                                            && row.profile == receipt.outcome.profile
                                            && row.agent_pane.state == crate::session::PanePresence::Alive
                                            && receipt.outcome.target.as_ref().is_some_and(|target|
                                                row.agent_pane.tmux_session.as_deref() == Some(target.tmux_session.as_str()))
                                            && matches!(row.status.as_str(), "Running" | "Waiting" | "Idle"),
                                        _ => true,
                                    },
                                    Some(target) => row
                                        .auxiliary
                                        .iter()
                                        .find(|observation| observation.target == *target)
                                        .map(|observation| &observation.pane)
                                        .is_some_and(|pane| {
                                            pane.state == crate::session::PanePresence::Alive
                                                && matches!(reply, CommandReply::Terminal(receipt)
                                                    if pane.tmux_session.as_deref()
                                                        == Some(receipt.outcome.tmux_session.as_str()))
                                        }),
                                }
                            });
                        if !alive {
                            let _ = terminal.send(Err(
                                "Prepared target no longer matches the applied snapshot".into(),
                            ));
                        } else {
                            let name = match pending.reply.take() {
                                Some(CommandReply::Terminal(receipt)) => Some(receipt.outcome.tmux_session),
                                Some(CommandReply::Restart(receipt)) => receipt.outcome.target.map(|target| target.tmux_session),
                                _ => None,
                            };
                            let _ = terminal.send(name.ok_or_else(|| "Prepared target unavailable".into()));
                        }
                    }
                    return false;
                }
            }
            true
        });
        errors
    }

    pub(crate) fn try_recv(&mut self) -> Result<SessionFeedResult, TryRecvError> {
        if !self.receiver.has_changed().unwrap_or(false) {
            return Err(TryRecvError::Empty);
        }
        self.receiver
            .borrow_and_update()
            .clone()
            .ok_or(TryRecvError::Empty)
    }

    #[cfg(test)]
    pub(crate) fn terminal_driver_for_test(
        &mut self,
    ) -> impl FnMut(Result<MutationReceipt<TerminalTarget>, String>) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |result| {
            let command = receiver.try_recv().expect("expected a native preparation");
            assert!(matches!(
                command.request,
                SessionRequest::EnsureAuxiliary { .. }
            ));
            assert!(command
                .result
                .send(result.map(CommandReply::Terminal))
                .is_ok());
        }
    }

    #[cfg(test)]
    pub(crate) fn restart_driver_for_test(
        &mut self,
    ) -> impl FnMut(
        Result<MutationReceipt<crate::daemon::RestartOutcome>, String>,
    ) -> Option<(String, crate::daemon::RestartSessionBody)> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |result| {
            let command = receiver.try_recv().ok()?;
            assert!(command
                .result
                .send(result.map(CommandReply::Restart))
                .is_ok());
            let SessionRequest::Mutation(SessionMutation::Restart(body)) = command.request else {
                panic!("expected restart");
            };
            Some((command.id, body))
        }
    }

    /// Test seam: drive the command lane and report what a real daemon would
    /// have received, without replying to a creation (it stays in flight).
    #[cfg(test)]
    pub(crate) fn creation_driver_for_test(&mut self) -> impl FnMut() -> Vec<String> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move || {
            let mut seen = Vec::new();
            while let Ok(command) = receiver.try_recv() {
                match command.request {
                    SessionRequest::Create(_) => seen.push(format!("create:{}", command.id)),
                    SessionRequest::CancelCreation => {
                        seen.push(format!("cancel:{}", command.id));
                        let _ = command
                            .result
                            .send(Ok(CommandReply::Mutation(RuntimeCursor {
                                epoch: "test".into(),
                                revision: 1,
                            })));
                    }
                    SessionRequest::Mutation(_)
                    | SessionRequest::EnsureAuxiliary { .. }
                    | SessionRequest::EnsureAgent { .. } => {}
                }
            }
            seen
        }
    }

    #[cfg(test)]
    pub(crate) fn publish_progress_for_test(&self, progress: Vec<CreationProgress>) {
        self.progress.send_replace(Some(progress));
    }

    #[cfg(test)]
    pub(crate) fn command_driver_for_test(
        &mut self,
    ) -> impl FnMut(Result<RuntimeCursor, String>) -> Option<(String, SessionMutation)> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        move |result| {
            let command = receiver.try_recv().ok()?;
            assert!(command
                .result
                .send(result.map(CommandReply::Mutation))
                .is_ok());
            let SessionRequest::Mutation(mutation) = command.request else {
                panic!("expected session mutation");
            };
            Some((command.id, mutation))
        }
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, result: SessionFeedResult) {
        self.sender.send_replace(Some(result));
    }
    #[cfg(test)]
    pub(crate) fn seeded_for_test(result: SessionFeedResult) -> Self {
        let feed = Self::new();
        feed.sender.send_replace(Some(result));
        feed
    }
}

impl Default for SessionFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SessionFeed {
    fn drop(&mut self) {
        set_grant(&self.grant, false);
        set_grant(&self.native_grant, false);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(epoch: &str, revision: u64) -> Arc<RuntimeSnapshot> {
        Arc::new(RuntimeSnapshot {
            cursor: RuntimeCursor {
                epoch: epoch.into(),
                revision,
            },
            contents: crate::daemon::RuntimeContents {
                health: crate::daemon::RuntimeHealth::Healthy,
                capabilities: crate::daemon::RuntimeCapabilities {
                    mutations: true,
                    native_interaction: true,
                },
                default_profile: "test".into(),
                sessions: vec![],
                profiles: vec![],
                workspace_ordering: vec![],
                global_projects: vec![],
            },
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_rejects_old_publications_and_reports_pending_outcomes() {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("old", 1)));
        feed.mark_snapshot_applied(snapshot("old", 1));
        let old_sender = feed.sender.clone();
        let (commands, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(commands);
        set_grant(&feed.grant, true);
        let body =
            serde_json::from_value(serde_json::json!({"path": "/test", "tool": "claude"})).unwrap();
        feed.create_session("pending".into(), body).unwrap();
        let pending = requests.try_recv().unwrap();

        feed.connect("test".into());
        feed.task.take().unwrap().abort();
        old_sender.send_replace(Some(SessionFeedResult::Snapshot(snapshot("old", 99))));
        assert!(matches!(feed.try_recv(), Err(TryRecvError::Empty)));
        assert!(!feed.mutations_available());
        assert!(feed.applied.is_none());
        let outcomes = feed.drain_creation_results();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, "pending");
        assert!(
            outcomes[0].1.is_err(),
            "reconnect must not silently forget a creation"
        );
        drop(pending);
    }

    #[test]
    fn acknowledged_change_waits_for_applied_snapshot_and_current_permission() {
        for receipt_first in [true, false] {
            for revoke in [false, true] {
                let initial = snapshot("test", 1);
                let mut feed =
                    SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(initial.clone()));
                feed.mark_snapshot_applied(initial);
                let (commands, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
                feed.commands = Some(commands);
                set_grant(&feed.grant, true);
                feed.submit("session".into(), SessionMutation::Stop)
                    .unwrap();
                let mut response = Some(requests.try_recv().unwrap().result);
                let mut acknowledge = || {
                    assert!(response
                        .take()
                        .unwrap()
                        .send(Ok(CommandReply::Mutation(RuntimeCursor {
                            epoch: "test".into(),
                            revision: 2,
                        })))
                        .is_ok());
                };
                if receipt_first {
                    acknowledge();
                }
                assert!(feed.drain_command_errors().is_empty());
                assert!(!feed.can_submit("session"));
                feed.publish_for_test(SessionFeedResult::Snapshot(snapshot("test", 2)));
                assert!(feed.drain_command_errors().is_empty());
                assert!(
                    !feed.can_submit("session"),
                    "unapplied snapshot released command"
                );
                feed.mark_snapshot_applied(snapshot("another-daemon", 99));
                assert!(feed.drain_command_errors().is_empty());
                assert!(
                    !feed.can_submit("session"),
                    "foreign epoch confirmed command"
                );
                let SessionFeedResult::Snapshot(applied) = feed.try_recv().unwrap() else {
                    panic!("expected canonical snapshot");
                };
                feed.mark_snapshot_applied(applied);
                if revoke {
                    set_grant(&feed.grant, false);
                    set_grant(&feed.grant, true);
                }
                if !receipt_first {
                    assert!(feed.drain_command_errors().is_empty());
                    assert!(
                        !feed.can_submit("session"),
                        "in-flight request lost exclusion"
                    );
                    acknowledge();
                }
                let errors = feed.drain_command_errors();
                if revoke {
                    assert_eq!(errors.len(), 1);
                    assert_eq!(errors[0].id, "session");
                } else {
                    assert!(errors.is_empty());
                }
                feed.submit("session".into(), SessionMutation::Stop)
                    .unwrap();
            }
        }
    }

    #[test]
    fn native_preparation_requires_current_permission_and_an_applied_live_target() {
        use crate::session::PanePresence;
        #[derive(Debug, Clone, Copy)]
        enum Invalidation {
            None,
            Cancel,
            Permission,
            Deleted,
            Presence(PanePresence),
            TargetName(Option<&'static str>),
        }
        for invalidation in [
            Invalidation::None,
            Invalidation::Cancel,
            Invalidation::Permission,
            Invalidation::Deleted,
            Invalidation::Presence(PanePresence::Absent),
            Invalidation::Presence(PanePresence::Dead),
            Invalidation::Presence(PanePresence::Unknown),
            Invalidation::TargetName(None),
            Invalidation::TargetName(Some("another-owned-target")),
        ] {
            let target = AuxiliaryTarget::Tool {
                tool_name: "shell".into(),
            };
            let mut initial = snapshot("test", 1);
            let row: crate::daemon::SessionResponse = serde_json::from_value(serde_json::json!({
                "id": "session", "profile": "test",
                "auxiliary": [{
                    "target": target, "state": "alive",
                    "tmux_session": "daemon-owned-target"
                }]
            }))
            .unwrap();
            Arc::make_mut(&mut initial).contents.sessions.push(row);
            let mut feed =
                SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(initial.clone()));
            feed.mark_snapshot_applied(initial.clone());
            let mut respond = feed.terminal_driver_for_test();
            set_grant(&feed.native_grant, false);
            assert!(feed
                .ensure_auxiliary("session".into(), target.clone(), None)
                .is_err());
            assert!(feed.can_submit("session"));
            set_grant(&feed.native_grant, true);
            let mut prepared = feed
                .ensure_auxiliary("session".into(), target, None)
                .unwrap();
            let mut next = initial;
            Arc::make_mut(&mut next).cursor.revision = 2;
            match invalidation {
                Invalidation::None => {}
                Invalidation::Cancel => prepared.lease.revoke(),
                Invalidation::Permission => {
                    set_grant(&feed.native_grant, false);
                    set_grant(&feed.native_grant, true);
                }
                Invalidation::Deleted => Arc::make_mut(&mut next).contents.sessions.clear(),
                Invalidation::TargetName(name) => {
                    let row = &mut Arc::make_mut(&mut next).contents.sessions[0];
                    let mut value = serde_json::to_value(&*row).unwrap();
                    value["auxiliary"][0]["tmux_session"] = serde_json::json!(name);
                    *row = serde_json::from_value(value).unwrap();
                }
                Invalidation::Presence(state) => {
                    Arc::make_mut(&mut next).contents.sessions[0].auxiliary[0]
                        .pane
                        .state = state
                }
            }
            assert!(feed.drain_command_errors().is_empty());
            assert!(
                !feed.can_submit("session"),
                "in-flight exclusion: {invalidation:?}"
            );
            respond(Ok(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: TerminalTarget {
                    tmux_session: "daemon-owned-target".into(),
                    status: crate::daemon::TerminalTargetStatus::Exists,
                },
            }));
            feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
            assert!(feed.drain_command_errors().is_empty());
            if !matches!(
                invalidation,
                Invalidation::Cancel | Invalidation::Permission
            ) {
                assert!(matches!(
                    prepared.result.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
            }
            feed.mark_snapshot_applied(next);
            assert!(feed.drain_command_errors().is_empty());
            let ready = prepared.result.try_recv().unwrap();
            assert_eq!(
                ready.is_ok(),
                matches!(invalidation, Invalidation::None),
                "{invalidation:?}"
            );
            feed.submit("session".into(), SessionMutation::Stop)
                .unwrap();
        }
    }
}

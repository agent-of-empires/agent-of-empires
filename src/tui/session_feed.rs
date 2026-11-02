//! Canonical runtime subscription. Disconnects retain the last displayed state.

#[cfg(test)]
use std::future::Future;

use std::{
    collections::{HashMap, HashSet, VecDeque},
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

pub(crate) enum SessionRequest {
    Mutation(SessionMutation),
    Trash(crate::daemon::TrashSessionBody),
    Purge(crate::daemon::DeleteSessionBody),
    Restore,
    Rename(crate::daemon::RenameSessionBody),
    SetWorktreeName(crate::daemon::SetWorktreeNameBody),
    AttachProject(crate::daemon::AttachProjectBody),
    EnsureAuxiliary {
        target: AuxiliaryTarget,
        size: Option<TerminalSize>,
        adoption: Option<crate::session::LegacyToolAdoption>,
    },
    EnsureAgent {
        size: Option<TerminalSize>,
    },
    CancelCreation,
    SmartRename,
}

pub(crate) enum SessionCommandOutcome {
    Trashed(MutationReceipt<crate::daemon::TrashOutcome>),
    Purged(MutationReceipt<crate::daemon::PurgeOutcome>),
    Restored(MutationReceipt<crate::daemon::RestoreOutcome>),
    Renamed(MutationReceipt<crate::daemon::RenameOutcome>),
    WorktreeEdited(MutationReceipt<crate::daemon::WorktreeEditOutcome>),
    ProjectAttached(MutationReceipt<crate::daemon::AttachProjectOutcome>),
}

#[derive(Debug)]
enum CommandReply {
    Mutation(RuntimeCursor),
    Terminal(MutationReceipt<TerminalTarget>),
    Restart(MutationReceipt<crate::daemon::RestartOutcome>),
    Created(Box<MutationReceipt<crate::daemon::SessionResponse>>),
    Trash(MutationReceipt<crate::daemon::TrashOutcome>),
    Purge(MutationReceipt<crate::daemon::PurgeOutcome>),
    Restore(MutationReceipt<crate::daemon::RestoreOutcome>),
    Rename(MutationReceipt<crate::daemon::RenameOutcome>),
    SmartRename(MutationReceipt<()>),
    WorktreeName(MutationReceipt<crate::daemon::WorktreeEditOutcome>),
    AttachProject(MutationReceipt<crate::daemon::AttachProjectOutcome>),
    Namespace(MutationReceipt<crate::daemon::NamespaceOutcome>),
    CommittedFailure {
        cursor: RuntimeCursor,
        failure: CommandFailure,
    },
}

impl CommandReply {
    fn cursor(&self) -> &RuntimeCursor {
        match self {
            Self::Mutation(cursor) => cursor,
            Self::Terminal(receipt) => &receipt.cursor,
            Self::Restart(receipt) => &receipt.cursor,
            Self::Created(receipt) => &receipt.cursor,
            Self::Trash(receipt) => &receipt.cursor,
            Self::Purge(receipt) => &receipt.cursor,
            Self::Restore(receipt) => &receipt.cursor,
            Self::Rename(receipt) => &receipt.cursor,
            Self::SmartRename(receipt) => &receipt.cursor,
            Self::WorktreeName(receipt) => &receipt.cursor,
            Self::AttachProject(receipt) => &receipt.cursor,
            Self::Namespace(receipt) => &receipt.cursor,
            Self::CommittedFailure { cursor, .. } => cursor,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CommandFailure {
    Rejected(String),
    Unknown(String),
}

impl CommandFailure {
    /// Classify a creation before any HTTP request was submitted. Only local
    /// client construction and definitive non-timeout 4xx responses prove a
    /// refusal; every transport/server/2xx-decoding failure is indeterminate.
    fn creation(error: crate::daemon::DaemonClientError) -> Self {
        Self::classify(error, false)
    }

    /// Classify an already-submitted mutation. A definitive 4xx (except 408)
    /// is the only server response that proves it was refused. Timeouts,
    /// transport failures, 5xx, receipt mismatches, and malformed 2xx bodies
    /// all leave the outcome unknown.
    fn submitted(error: crate::daemon::DaemonClientError) -> Self {
        Self::classify(error, true)
    }

    fn classify(error: crate::daemon::DaemonClientError, submitted: bool) -> Self {
        use crate::daemon::DaemonClientError;
        let rejected = match &error {
            DaemonClientError::Status { status, .. } => {
                status.is_client_error() && *status != reqwest::StatusCode::REQUEST_TIMEOUT
            }
            DaemonClientError::InvalidBaseUrl { .. }
            | DaemonClientError::InvalidPathSegment
            | DaemonClientError::InvalidBearerToken
            | DaemonClientError::InvalidSessionCredential
            | DaemonClientError::InsecureCredentialTransport
            | DaemonClientError::ClientBuild => !submitted,
            DaemonClientError::Timeout
            | DaemonClientError::Transport
            | DaemonClientError::ResponseTooLarge { .. }
            | DaemonClientError::Decode(_)
            | DaemonClientError::AuthenticatedDecode
            | DaemonClientError::InvalidMutationReceipt
            | DaemonClientError::Committed { .. } => false,
            DaemonClientError::UnixTransport | DaemonClientError::PeerIdentity => false,
        };
        if rejected {
            Self::Rejected(error.to_string())
        } else {
            Self::Unknown(error.to_string())
        }
    }

    fn message(self) -> String {
        match self {
            Self::Rejected(message) | Self::Unknown(message) => message,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NativeLease {
    grant: Arc<AtomicU64>,
    generation: u64,
    cancelled: Arc<AtomicBool>,
    scope: Arc<std::sync::OnceLock<NativeInputScope>>,
    latest: Option<tokio::sync::watch::Receiver<Option<SessionFeedResult>>>,
    preparation: Option<Arc<NativePreparationScope>>,
}

struct NativePreparationScope {
    id: String,
    epoch: String,
    identity: crate::daemon::NativePreparationIdentity,
    auxiliary: Option<AuxiliaryTarget>,
    pane: crate::session::PaneObservation,
    profile_binding: Option<Arc<crate::session::Storage>>,
}

impl std::fmt::Debug for NativePreparationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativePreparationScope")
            .field("id", &self.id)
            .field("identity", &self.identity)
            .field("auxiliary", &self.auxiliary)
            .finish_non_exhaustive()
    }
}

impl NativePreparationScope {
    fn owner<'a>(
        &self,
        snapshot: &'a RuntimeSnapshot,
    ) -> Option<&'a crate::daemon::SessionResponse> {
        (snapshot.cursor.epoch == self.epoch)
            .then(|| {
                snapshot.contents.sessions.iter().find(|row| {
                    row.id == self.id
                        && row.created_at == self.identity.created_at
                        && row.profile == self.identity.profile
                })
            })
            .flatten()
    }

    fn matches_initial(&self, snapshot: &RuntimeSnapshot) -> bool {
        self.owner(snapshot).is_some_and(|row| {
            let pane = self
                .auxiliary
                .as_ref()
                .map_or(Some(&row.agent_pane), |target| {
                    row.auxiliary
                        .iter()
                        .find(|observation| observation.target == *target)
                        .map(|observation| &observation.pane)
                });
            let fixed = self.pane.physical.is_some() || self.pane.legacy_tool.is_some();
            row.lifecycle_generation == self.identity.lifecycle_generation
                && (!fixed
                    || pane.is_some_and(|pane| {
                        (self.pane.physical.is_some()
                            || pane.tmux_session == self.pane.tmux_session)
                            && pane.physical == self.pane.physical
                            && pane.legacy_tool == self.pane.legacy_tool
                    }))
        })
    }
}

#[derive(Debug)]
struct NativeInputScope {
    id: String,
    epoch: String,
    created_at: String,
    auxiliary: Option<AuxiliaryTarget>,
    target: Arc<crate::daemon::TerminalTarget>,
    checked_revision: AtomicU64,
    input_fence: std::sync::Mutex<Option<Arc<crate::session::NativeInputFence>>>,
}

impl NativeInputScope {
    fn matches(&self, snapshot: &RuntimeSnapshot) -> bool {
        snapshot.cursor.epoch == self.epoch
            && snapshot.contents.sessions.iter().any(|row| {
                if row.id != self.id
                    || row.profile != self.target.profile
                    || row.lifecycle_generation != self.target.lifecycle_generation
                    || row.created_at != self.created_at
                {
                    return false;
                }
                let pane = match &self.auxiliary {
                    None => &row.agent_pane,
                    Some(target) => {
                        let Some(observation) = row
                            .auxiliary
                            .iter()
                            .find(|observation| observation.target == *target)
                        else {
                            return false;
                        };
                        &observation.pane
                    }
                };
                pane.state == crate::session::PanePresence::Alive
                    && pane.legacy_tool.is_none()
                    && self.target.physical.is_some()
                    && pane.physical == self.target.physical
            })
    }

    fn accepts(&self, snapshot: &RuntimeSnapshot) -> bool {
        if snapshot.cursor.epoch != self.epoch {
            return false;
        }
        if self.checked_revision.load(Ordering::SeqCst) == snapshot.cursor.revision {
            return true;
        }
        if !self.matches(snapshot) {
            return false;
        }
        self.checked_revision
            .store(snapshot.cursor.revision, Ordering::SeqCst);
        true
    }
}

impl NativeLease {
    pub(crate) fn is_valid(&self) -> bool {
        if self.cancelled.load(Ordering::SeqCst)
            || self.grant.load(Ordering::SeqCst) != self.generation
        {
            return false;
        }
        if let Some(scope) = self.scope.get() {
            let current = self.latest.as_ref().is_some_and(|latest| {
                matches!(&*latest.borrow(), Some(SessionFeedResult::Snapshot(snapshot))
                    if scope.accepts(snapshot))
            });
            if !current {
                self.cancel_continuation();
                return false;
            }
        }
        if self.scope.get().is_none() {
            if let Some(preparation) = &self.preparation {
                let current = self.latest.as_ref().is_some_and(|latest| {
                    matches!(&*latest.borrow(), Some(SessionFeedResult::Snapshot(snapshot))
                        if preparation.owner(snapshot).is_some())
                });
                if !current {
                    self.cancel_continuation();
                    return false;
                }
            }
        }
        !self.cancelled.load(Ordering::SeqCst)
            && self.grant.load(Ordering::SeqCst) == self.generation
    }

    fn can_dispatch(&self) -> bool {
        let current = self.is_valid()
            && self.preparation.as_ref().is_none_or(|preparation| {
                preparation
                    .profile_binding
                    .as_ref()
                    .is_none_or(|storage| preparation.identity.matches_source(storage))
                    && self.latest.as_ref().is_some_and(|latest| {
                        matches!(&*latest.borrow(), Some(SessionFeedResult::Snapshot(snapshot))
                    if preparation.matches_initial(snapshot))
                    })
            });
        if !current {
            self.cancel_continuation();
        }
        current
    }

    pub(crate) fn preparation_profile_identity(&self) -> Option<(u64, u64)> {
        self.preparation
            .as_ref()
            .and_then(|preparation| preparation.identity.profile_identity)
    }

    fn bind(
        &self,
        id: &str,
        auxiliary: Option<AuxiliaryTarget>,
        target: Arc<crate::daemon::TerminalTarget>,
        snapshot: &RuntimeSnapshot,
        latest: Option<&SessionFeedResult>,
    ) -> bool {
        if self.cancelled.load(Ordering::SeqCst)
            || self.grant.load(Ordering::SeqCst) != self.generation
        {
            return false;
        }
        if self.preparation.as_ref().is_some_and(|preparation| {
            preparation
                .profile_binding
                .as_ref()
                .is_some_and(|storage| !preparation.identity.matches_source(storage))
        }) {
            return false;
        }
        if self.preparation.as_ref().is_some_and(|preparation| {
            preparation.id != id
                || preparation.auxiliary != auxiliary
                || preparation.owner(snapshot).is_none()
        }) {
            return false;
        }
        if let Some(scope) = self.scope.get() {
            return scope.id == id
                && scope.auxiliary == auxiliary
                && scope.target.lifecycle_generation == target.lifecycle_generation
                && scope.target.profile == target.profile
                && scope.target.physical == target.physical
                && scope.matches(snapshot)
                && matches!(latest, Some(SessionFeedResult::Snapshot(latest)) if scope.matches(latest));
        }
        let Some(row) = snapshot.contents.sessions.iter().find(|row| row.id == id) else {
            return false;
        };
        let scope = NativeInputScope {
            id: id.to_owned(),
            epoch: snapshot.cursor.epoch.clone(),
            created_at: row.created_at.clone(),
            auxiliary,
            target,
            checked_revision: AtomicU64::new(snapshot.cursor.revision),
            input_fence: std::sync::Mutex::new(None),
        };
        if !scope.matches(snapshot) {
            return false;
        }
        let Some(SessionFeedResult::Snapshot(latest)) = latest else {
            return false;
        };
        if !scope.matches(latest) {
            return false;
        }
        scope
            .checked_revision
            .store(latest.cursor.revision, Ordering::SeqCst);
        self.scope.set(scope).is_ok()
    }

    pub(crate) fn target(&self) -> Option<&Arc<crate::daemon::TerminalTarget>> {
        self.scope.get().map(|scope| &scope.target)
    }

    pub(crate) fn input_fence(
        &self,
        agent: bool,
    ) -> anyhow::Result<Arc<crate::session::NativeInputFence>> {
        use crate::session::NativeInputError;
        anyhow::ensure!(self.is_valid(), NativeInputError::Ownership);
        let scope = self.scope.get().ok_or(NativeInputError::Ownership)?;
        let expected_profile = self
            .preparation
            .as_ref()
            .map(|preparation| {
                preparation
                    .identity
                    .profile_identity
                    .ok_or(NativeInputError::Ownership)
            })
            .transpose()?;
        let capture = || {
            let created = chrono::DateTime::parse_from_rfc3339(&scope.created_at)
                .map_err(|_| NativeInputError::Ownership)?
                .with_timezone(&chrono::Utc);
            crate::session::NativeInputFence::capture(
                &scope.target.profile,
                &scope.id,
                created,
                scope.target.lifecycle_generation,
                agent,
                expected_profile,
            )
        };
        // Programmatic Agent admission has its own stronger policy; never
        // recycle a raw-human fence that deliberately has no Agent readiness rule.
        if agent {
            return capture();
        }
        let mut cached = scope
            .input_fence
            .lock()
            .map_err(|_| NativeInputError::Ownership)?;
        if let Some(fence) = cached.as_ref() {
            return Ok(fence.clone());
        }
        let fence = capture()?;
        *cached = Some(fence.clone());
        Ok(fence)
    }

    /// Capture before handing any bytes to a worker queue; effects recheck native custody.
    pub(crate) fn input_session(&self) -> anyhow::Result<Arc<crate::tmux::Session>> {
        use crate::session::NativeInputError;
        let fence = self.input_fence(false)?;
        let scope = self.scope.get().ok_or(NativeInputError::Ownership)?;
        let physical = scope
            .target
            .physical
            .clone()
            .ok_or(NativeInputError::Ownership)?;
        let session = crate::tmux::Session::from_observation(&scope.target.tmux_session, physical)
            .with_native_input_fence(fence)
            .capture_input_channel();
        anyhow::ensure!(self.is_valid(), NativeInputError::Ownership);
        Ok(Arc::new(session))
    }

    #[cfg(test)]
    pub(crate) fn bound_for_test(
        instance: &crate::session::Instance,
        auxiliary: Option<AuxiliaryTarget>,
        target: Arc<crate::daemon::TerminalTarget>,
    ) -> Self {
        let pane = crate::session::PaneObservation {
            state: crate::session::PanePresence::Alive,
            tmux_session: Some(target.tmux_session.clone()),
            physical: target.physical.clone(),
            legacy_tool: None,
        };
        let observations: Vec<_> = auxiliary
            .iter()
            .map(|target| crate::session::AuxiliaryObservation {
                target: target.clone(),
                pane: pane.clone(),
            })
            .collect();
        let row = serde_json::from_value(serde_json::json!({
            "id": instance.id, "profile": target.profile,
            "created_at": instance.created_at.to_rfc3339(),
            "lifecycle_generation": instance.lifecycle_generation,
            "status": instance.status,
            "agent_pane": if auxiliary.is_none() { pane } else { Default::default() },
            "auxiliary": observations,
        }))
        .expect("bound native fixture row");
        let snapshot = Arc::new(RuntimeSnapshot {
            cursor: RuntimeCursor {
                epoch: "native-input-test".into(),
                revision: 1,
            },
            contents: crate::daemon::RuntimeContents {
                health: crate::daemon::RuntimeHealth::Healthy,
                capabilities: crate::daemon::RuntimeCapabilities {
                    mutations: true,
                    native_interaction: true,
                },
                default_profile: target.profile.clone(),
                sessions: vec![row],
                profiles: vec![],
                workspace_ordering: vec![],
                global_projects: vec![],
            },
        });
        let latest = SessionFeedResult::Snapshot(snapshot.clone());
        let (_sender, receiver) = tokio::sync::watch::channel(Some(latest.clone()));
        let mut lease = Self::valid_for_test();
        lease.latest = Some(receiver);
        assert!(lease.bind(&instance.id, auxiliary, target, &snapshot, Some(&latest)));
        lease
    }

    pub(crate) fn cancel_continuation(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn valid_for_test() -> Self {
        Self {
            grant: Arc::new(AtomicU64::new(1)),
            generation: 1,
            cancelled: Arc::new(AtomicBool::new(false)),
            scope: Arc::new(std::sync::OnceLock::new()),
            preparation: None,
            latest: None,
        }
    }
}

pub(crate) struct NativePreparation {
    pub lease: NativeLease,
    pub result: tokio::sync::oneshot::Receiver<Result<Arc<crate::daemon::TerminalTarget>, String>>,
}

enum CommandRequest {
    Session { id: String, request: SessionRequest },
    Create(Box<CreateSessionBody>),
    Namespace(crate::daemon::NamespaceMutation),
}

struct SessionCommand {
    request: CommandRequest,
    lease: u64,
    native: Option<NativeLease>,
    result: tokio::sync::oneshot::Sender<Result<CommandReply, CommandFailure>>,
}

struct PendingNamespace {
    grant: Arc<AtomicU64>,
    lease: u64,
    result: tokio::sync::oneshot::Receiver<Result<CommandReply, CommandFailure>>,
    reply: Option<MutationReceipt<crate::daemon::NamespaceOutcome>>,
}

struct PendingTerminal {
    /// `None` means the session's own agent pane, which the snapshot reports
    /// through `agent_pane` rather than the auxiliary list.
    target: Option<AuxiliaryTarget>,
    native: NativeLease,
    result: tokio::sync::oneshot::Sender<Result<Arc<crate::daemon::TerminalTarget>, String>>,
}

struct DeferredNativePreparation {
    request: SessionRequest,
    native: NativeLease,
    terminal: PendingTerminal,
    epoch: String,
    profile: String,
    lifecycle_generation: u64,
    created_at: String,
}

impl DeferredNativePreparation {
    fn matches(&self, id: &str, snapshot: &RuntimeSnapshot) -> bool {
        snapshot.cursor.epoch == self.epoch
            && snapshot.contents.sessions.iter().any(|row| {
                row.id == id
                    && row.profile == self.profile
                    && row.lifecycle_generation == self.lifecycle_generation
                    && row.created_at == self.created_at
            })
    }

    fn reject(self, message: &str) {
        let _ = self.terminal.result.send(Err(message.into()));
    }
}

struct PendingCommand {
    grant: Arc<AtomicU64>,
    lease: u64,
    result: tokio::sync::oneshot::Receiver<Result<CommandReply, CommandFailure>>,
    reply: Option<CommandReply>,
    terminal: Option<PendingTerminal>,
    marks_unread: bool,
    is_engagement: bool,
    deferred_native: Option<Box<DeferredNativePreparation>>,
    diff_save: Option<tokio::sync::oneshot::Sender<Result<RuntimeCursor, CommandFailure>>>,
}

impl PendingCommand {
    fn fail(&mut self, id: &str, failure: CommandFailure, errors: &mut Vec<SessionCommandError>) {
        let unknown = matches!(&failure, CommandFailure::Unknown(_));
        let diff_failure = self.diff_save.take();
        if let Some(result) = diff_failure {
            let _ = result.send(Err(failure.clone()));
        }
        let message = failure.message();
        if let Some(deferred) = self.deferred_native.take() {
            deferred.reject(&message);
        }
        if let Some(terminal) = self.terminal.take() {
            let _ = terminal.result.send(Err(message.clone()));
            if !unknown {
                return;
            }
            // An unknown outcome blocks the row until it is resolved, and the
            // queued error is the only path to that prompt. A native command
            // carrying a terminal must queue it too, or the row stays blocked
            // with nothing to unblock it.
        }
        errors.push(SessionCommandError {
            id: id.into(),
            message,
            marks_unread: self.marks_unread,
            outcome_unknown: unknown,
        });
    }
}

pub(crate) struct SessionCommandError {
    pub id: String,
    pub message: String,
    pub marks_unread: bool,
    pub(crate) outcome_unknown: bool,
}

/// One in-flight daemon creation, correlated with the caller's own row by
/// `token` because the session id does not exist until the daemon commits.
struct PendingCreation {
    grant: Arc<AtomicU64>,
    lease: u64,
    result: tokio::sync::oneshot::Receiver<Result<CommandReply, CommandFailure>>,
    receipt: Option<Box<MutationReceipt<crate::daemon::SessionResponse>>>,
}

fn set_grant(grant: &AtomicU64, allowed: bool) {
    // Renamed to try_update in the standard library; the MSRV this builds against
    // does not have it yet. Upstream carries the same allowance.
    #[allow(deprecated)]
    let _ = grant.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        ((value & 1 == 1) != allowed).then(|| value + 1)
    });
}
async fn run_namespace(
    client: &crate::daemon::DaemonClient,
    request: crate::daemon::NamespaceMutation,
    epoch: &str,
) -> Result<MutationReceipt<crate::daemon::NamespaceOutcome>, crate::daemon::DaemonClientError> {
    use crate::daemon::{NamespaceMutation as N, NamespaceOutcome as O};
    match request {
        N::CollapseGroup(body) => {
            client
                .collapse_group(&body, epoch)
                .await
                .map(|cursor| MutationReceipt {
                    cursor,
                    outcome: O::Committed,
                })
        }
        N::MoveGroup(body) => client
            .move_group(&body, epoch)
            .await
            .map(|cursor| MutationReceipt {
                cursor,
                outcome: O::Committed,
            }),
        N::DeleteGroup(body) => client
            .delete_group(&body, epoch)
            .await
            .map(|r| MutationReceipt {
                cursor: r.cursor,
                outcome: O::DeletedGroup(r.outcome),
            }),
        N::Reorder(body) => client.reorder(&body, epoch).await.map(|r| MutationReceipt {
            cursor: r.cursor,
            outcome: O::Reordered(r.outcome),
        }),
        N::Profile(body) => {
            client
                .mutate_profile(&body, epoch)
                .await
                .map(|cursor| MutationReceipt {
                    cursor,
                    outcome: O::Committed,
                })
        }
    }
}

async fn run_command_writer(
    client: crate::daemon::DaemonClient,
    epoch: String,
    grant: Arc<AtomicU64>,
    mut requests: tokio::sync::mpsc::Receiver<SessionCommand>,
    #[cfg(test)] creation_count: Option<tokio::sync::watch::Sender<usize>>,
) {
    let mut creations = tokio::task::JoinSet::new();
    loop {
        let request = tokio::select! {
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
            _ = creations.join_next(), if !creations.is_empty() => {
                #[cfg(test)]
                if let Some(count) = &creation_count {
                    count.send_replace(creations.len());
                }
                continue;
            }
        };
        let allowed = request.native.as_ref().map_or_else(
            || grant.load(Ordering::SeqCst) == request.lease,
            NativeLease::can_dispatch,
        );
        let expected = if allowed {
            request.native.as_ref().and_then(|native| {
                native
                    .preparation
                    .as_ref()
                    .map(|preparation| preparation.identity.clone())
            })
        } else {
            None
        };
        if allowed && matches!(&request.request, CommandRequest::Create(_)) {
            let SessionCommand {
                request: CommandRequest::Create(body),
                result,
                ..
            } = request
            else {
                unreachable!()
            };
            let client = client.clone();
            let epoch = epoch.clone();
            creations.spawn(async move {
                let outcome = client
                    .create_session(&body, &epoch)
                    .await
                    .map(|receipt| CommandReply::Created(Box::new(receipt)))
                    .map_err(CommandFailure::creation);
                let _ = result.send(outcome);
            });
            #[cfg(test)]
            if let Some(count) = &creation_count {
                count.send_replace(creations.len());
            }
            continue;
        }
        let result = if !allowed {
            Err(CommandFailure::Rejected(
                "Request not submitted: runtime permission changed or continuation was cancelled"
                    .into(),
            ))
        } else {
            match request.request {
                CommandRequest::Create(_) => unreachable!(),
                CommandRequest::Namespace(mutation) => run_namespace(&client, mutation, &epoch)
                    .await
                    .map(CommandReply::Namespace),
                CommandRequest::Session { id, request } => match request {
                    SessionRequest::SmartRename => client
                        .smart_rename_session(&id, &epoch)
                        .await
                        .map(|receipt| {
                            CommandReply::SmartRename(MutationReceipt {
                                cursor: receipt.cursor,
                                outcome: (),
                            })
                        }),
                    SessionRequest::Trash(body) => client
                        .trash_session(&id, &body, &epoch)
                        .await
                        .map(CommandReply::Trash),
                    SessionRequest::Purge(body) => client
                        .purge_session(&id, &body, &epoch)
                        .await
                        .map(CommandReply::Purge),
                    SessionRequest::Restore => client
                        .restore_session(&id, &epoch)
                        .await
                        .map(CommandReply::Restore),
                    SessionRequest::Rename(body) => client
                        .rename_session(&id, &body, &epoch)
                        .await
                        .map(CommandReply::Rename),
                    SessionRequest::SetWorktreeName(body) => client
                        .set_worktree_name(&id, &body, &epoch)
                        .await
                        .map(CommandReply::WorktreeName),
                    SessionRequest::AttachProject(body) => client
                        .attach_session_project(&id, &body, &epoch)
                        .await
                        .map(CommandReply::AttachProject),
                    SessionRequest::Mutation(SessionMutation::Restart(body)) => client
                        .restart_session(&id, &body, &epoch)
                        .await
                        .map(CommandReply::Restart),
                    SessionRequest::Mutation(mutation) => client
                        .mutate_session(&id, &mutation, &epoch)
                        .await
                        .map(CommandReply::Mutation),
                    SessionRequest::EnsureAuxiliary {
                        target,
                        size,
                        adoption,
                    } => {
                        let receipt = match target {
                            AuxiliaryTarget::Host { index } => {
                                client
                                    .ensure_terminal(
                                        &id,
                                        index,
                                        &StartSessionBody { expected, size },
                                        &epoch,
                                    )
                                    .await
                            }
                            AuxiliaryTarget::Container { index } => {
                                client
                                    .ensure_container_terminal(
                                        &id,
                                        index,
                                        &StartSessionBody { expected, size },
                                        &epoch,
                                    )
                                    .await
                            }
                            AuxiliaryTarget::Tool { tool_name } => {
                                client
                                    .ensure_tool(
                                        &id,
                                        &EnsureToolBody {
                                            expected,
                                            tool_name,
                                            size,
                                            adoption,
                                        },
                                        &epoch,
                                    )
                                    .await
                            }
                        };
                        receipt.map(CommandReply::Terminal)
                    }
                    SessionRequest::EnsureAgent { size } => client
                        .ensure_agent(&id, &StartSessionBody { expected, size }, &epoch)
                        .await
                        .map(CommandReply::Terminal),
                    SessionRequest::CancelCreation => client
                        .cancel_creation(&id, &epoch)
                        .await
                        .map(CommandReply::Mutation),
                },
            }
            .or_else(|error| match error {
                error @ crate::daemon::DaemonClientError::Committed { .. } => {
                    let message = error.to_string();
                    let crate::daemon::DaemonClientError::Committed { cursor, code } = error else {
                        unreachable!();
                    };
                    let failure = if matches!(
                        code,
                        crate::daemon::ApiErrorCode::PurgeFailed
                            | crate::daemon::ApiErrorCode::ManagedResumeRefused(_)
                    ) {
                        CommandFailure::Rejected(message)
                    } else {
                        CommandFailure::Unknown(message)
                    };
                    Ok(CommandReply::CommittedFailure { cursor, failure })
                }
                error => Err(CommandFailure::submitted(error)),
            })
        };
        let _ = request.result.send(result);
    }
    creations.shutdown().await;
}
#[derive(Clone)]
struct QuitCancellationHandle {
    client: crate::daemon::DaemonClient,
    epoch: String,
    grant: Arc<AtomicU64>,
}

pub struct SessionFeed {
    sender: tokio::sync::watch::Sender<Option<SessionFeedResult>>,
    receiver: tokio::sync::watch::Receiver<Option<SessionFeedResult>>,
    /// Advisory creation progress, on its own channel so a busy hook cannot
    /// overwrite an unapplied canonical snapshot.
    progress: tokio::sync::watch::Sender<Option<Vec<CreationProgress>>>,
    progress_receiver: tokio::sync::watch::Receiver<Option<Vec<CreationProgress>>>,
    diagnostics: tokio::sync::watch::Sender<Vec<crate::daemon::OwnerLoadDiagnostic>>,
    diagnostics_receiver: tokio::sync::watch::Receiver<Vec<crate::daemon::OwnerLoadDiagnostic>>,
    quit_control: tokio::sync::watch::Sender<Option<QuitCancellationHandle>>,
    task: Option<tokio::task::JoinHandle<()>>,
    grant: Arc<AtomicU64>,
    native_grant: Arc<AtomicU64>,
    /// One preview channel's native witness; forwarded mouse/key actions reuse it.
    preview_input: std::sync::Mutex<Option<NativeLease>>,
    /// CityHall client mode is a handshake property rather than a per-revision
    /// grant, so it is recorded once per connection and read by the local-write
    /// gate on the home view.
    cityhall: Arc<AtomicBool>,
    commands: Option<tokio::sync::mpsc::Sender<SessionCommand>>,
    pending: HashMap<String, PendingCommand>,
    pending_creations: HashMap<String, PendingCreation>,
    pending_namespace: Option<PendingNamespace>,
    namespace_quarantined: bool,
    row_outcomes: Vec<(String, SessionCommandOutcome)>,
    bulk_queue: VecDeque<(String, SessionRequest)>,
    bulk_errors: Vec<SessionCommandError>,
    applied: Option<Arc<RuntimeSnapshot>>,
    /// Sessions whose last submitted mutation had an unknown outcome. A
    /// reconnect alone cannot prove the old request was or was not applied, so
    /// the fence survives reconnect and is deliberately operator-resolved.
    quarantined: HashSet<String>,
}

impl SessionFeed {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (progress, progress_receiver) = tokio::sync::watch::channel(None);
        let (quit_control, _) = tokio::sync::watch::channel(None);
        let (diagnostics, diagnostics_receiver) = tokio::sync::watch::channel(Vec::new());
        Self {
            sender,
            receiver,
            progress,
            progress_receiver,
            quit_control,
            task: None,
            diagnostics,
            diagnostics_receiver,
            grant: Arc::new(AtomicU64::new(0)),
            native_grant: Arc::new(AtomicU64::new(0)),
            preview_input: std::sync::Mutex::new(None),
            cityhall: Arc::new(AtomicBool::new(false)),
            commands: None,
            pending: HashMap::new(),
            pending_creations: HashMap::new(),
            pending_namespace: None,
            namespace_quarantined: false,
            row_outcomes: Vec::new(),
            applied: None,
            bulk_errors: Vec::new(),
            quarantined: HashSet::new(),
            bulk_queue: VecDeque::new(),
        }
    }

    pub fn connect(&mut self, profile: String) {
        self.connect_prepared(profile, None);
    }

    pub(crate) fn suspend_for_dashboard(&mut self) {
        set_grant(&self.grant, false);
        set_grant(&self.native_grant, false);
        self.quit_control.send_replace(None);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.commands = None;
    }

    pub(crate) fn connect_prepared(
        &mut self,
        profile: String,
        prepared: Option<(RuntimeConnection, crate::daemon::DaemonClient)>,
    ) {
        set_grant(&self.grant, false);
        set_grant(&self.native_grant, false);
        self.quit_control.send_replace(None);
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
        let cityhall = Arc::new(AtomicBool::new(false));
        self.cityhall = cityhall.clone();
        let (commands, requests) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(commands);
        let (progress, progress_receiver) = tokio::sync::watch::channel(None);
        self.progress = progress.clone();
        self.progress_receiver = progress_receiver;
        let sender = self.sender.clone();
        let quit_control = self.quit_control.clone();
        let diagnostics = self.diagnostics.clone();
        self.task = Some(tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let (mut connection, client) = match prepared {
                    Some(prepared) => prepared,
                    None => {
                        let endpoint =
                            crate::acp::client::daemon_manager::ensure_native_daemon(&profile)
                                .await?;
                        let connection = RuntimeConnection::connect(
                            &endpoint,
                            (!profile.is_empty()).then_some(profile.as_str()),
                        )
                        .await?;
                        let client = crate::acp::client::HttpClient::new(endpoint)?
                            .daemon_client()
                            .await?;
                        (connection, client)
                    }
                };
                let epoch = connection.info().epoch.clone();
                diagnostics.send_replace(connection.info().owner_diagnostics.clone());
                cityhall.store(connection.info().cityhall_mode, Ordering::SeqCst);
                set_grant(&grant, connection.mutations_allowed());
                set_grant(&native_grant, connection.native_interaction_allowed());
                quit_control.send_replace(Some(QuitCancellationHandle {
                    client: client.clone(),
                    epoch: epoch.clone(),
                    grant: grant.clone(),
                }));
                sender.send_replace(Some(SessionFeedResult::Snapshot(
                    connection.snapshot().clone(),
                )));
                progress.send_replace(Some(connection.creation_progress().to_vec()));
                let read = async {
                    loop {
                        match connection.next_event().await {
                            Ok(RuntimeEvent::Snapshot(snapshot)) => {
                                set_grant(&grant, connection.mutations_allowed());
                                set_grant(&native_grant, connection.native_interaction_allowed());
                                sender.send_replace(Some(SessionFeedResult::Snapshot(snapshot)));
                            }
                            Ok(RuntimeEvent::OwnerDiagnostics(current)) => {
                                diagnostics.send_replace(current);
                            }
                            Ok(RuntimeEvent::Progress(progresses)) => {
                                progress.send_replace(Some(progresses));
                            }
                            Err(error) => {
                                set_grant(&grant, false);
                                set_grant(&native_grant, false);
                                progress.send_replace(Some(Vec::new()));
                                sender.send_replace(Some(SessionFeedResult::Unavailable(
                                    error.to_string(),
                                )));
                                break;
                            }
                        }
                    }
                };
                let write = run_command_writer(
                    client,
                    epoch,
                    grant.clone(),
                    requests,
                    #[cfg(test)]
                    None,
                );
                tokio::join!(read, write);
                Ok(())
            }
            .await;
            set_grant(&grant, false);
            set_grant(&native_grant, false);
            quit_control.send_replace(None);
            if let Err(error) = result {
                sender.send_replace(Some(SessionFeedResult::Unavailable(error.to_string())));
            }
        }));
    }
    pub(crate) fn take_diagnostics(&mut self) -> Option<Vec<crate::daemon::OwnerLoadDiagnostic>> {
        self.diagnostics_receiver
            .has_changed()
            .unwrap_or(false)
            .then(|| self.diagnostics_receiver.borrow_and_update().clone())
    }

    pub(crate) fn latest_snapshot(&self) -> Option<Arc<RuntimeSnapshot>> {
        match self.receiver.borrow().as_ref() {
            Some(SessionFeedResult::Snapshot(snapshot)) => Some(snapshot.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn native_lease_for_test(&self) -> NativeLease {
        NativeLease {
            grant: self.native_grant.clone(),
            generation: self.native_grant.load(Ordering::SeqCst),
            cancelled: Arc::new(AtomicBool::new(false)),
            scope: Arc::new(std::sync::OnceLock::new()),
            preparation: None,
            latest: Some(self.receiver.clone()),
        }
    }

    pub(crate) fn mutations_available(&self) -> bool {
        self.grant.load(Ordering::SeqCst) & 1 == 1
            && self
                .commands
                .as_ref()
                .is_some_and(|commands| !commands.is_closed())
    }

    /// True when the attached runtime serves a CityHall client, which enforces
    /// a narrower mutation policy than the local owner this process runs as.
    pub(crate) fn cityhall_mode(&self) -> bool {
        self.cityhall.load(Ordering::SeqCst)
    }

    pub(crate) fn native_interaction_available(&self) -> bool {
        self.native_grant.load(Ordering::SeqCst) & 1 == 1
            && self
                .commands
                .as_ref()
                .is_some_and(|commands| !commands.is_closed())
    }

    /// A fresh human key aimed at an already displayed pane does not authorize
    /// starting anything. Bind the applied physical observation before queuing it.
    pub(crate) fn preview_input_lease(&self, id: &str, name: &str) -> Option<NativeLease> {
        if !self.native_interaction_available() || !self.current_snapshot_applied() {
            return None;
        }
        let mut cached = self.preview_input.lock().ok()?;
        if let Some(lease) = cached.as_ref() {
            if lease
                .scope
                .get()
                .is_some_and(|scope| scope.id == id && scope.target.tmux_session == name)
                && lease.is_valid()
            {
                return Some(lease.clone());
            }
        }
        let snapshot = self.applied.as_ref()?;
        let row = snapshot.contents.sessions.iter().find(|row| row.id == id)?;
        let (auxiliary, pane) = if row.agent_pane.tmux_session.as_deref() == Some(name) {
            (None, &row.agent_pane)
        } else {
            let observation = row
                .auxiliary
                .iter()
                .find(|observation| observation.pane.tmux_session.as_deref() == Some(name))?;
            (Some(observation.target.clone()), &observation.pane)
        };
        if pane.state != crate::session::PanePresence::Alive
            || pane.legacy_tool.is_some()
            || pane.physical.is_none()
        {
            return None;
        }
        let target = Arc::new(crate::daemon::TerminalTarget {
            tmux_session: name.to_owned(),
            status: crate::daemon::TerminalTargetStatus::Alive,
            lifecycle_generation: row.lifecycle_generation,
            profile: row.profile.clone(),
            physical: pane.physical.clone(),
        });
        let lease = NativeLease {
            grant: self.native_grant.clone(),
            generation: self.native_grant.load(Ordering::SeqCst),
            cancelled: Arc::new(AtomicBool::new(false)),
            scope: Arc::new(std::sync::OnceLock::new()),
            preparation: None,
            latest: Some(self.receiver.clone()),
        };
        if !lease.bind(
            id,
            auxiliary,
            target,
            snapshot,
            self.receiver.borrow().as_ref(),
        ) {
            return None;
        }
        *cached = Some(lease.clone());
        Some(lease)
    }

    pub(crate) fn can_submit(&self, id: &str) -> bool {
        self.mutations_available()
            && !self.namespace_blocked()
            && !self.quarantined.contains(id)
            && !self.pending.contains_key(id)
            && self.pending.len() < COMMAND_CAPACITY
            && !self.bulk_queue.iter().any(|(queued, _)| queued == id)
    }

    /// Operator-confirmed resolution of an unknown mutation outcome. The
    /// current applied canonical snapshot proves the view is synchronized, but
    /// deliberately does not infer the lost mutation's result.
    pub(crate) fn resolve_indeterminate(&mut self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mutations_available(),
            "Runtime disconnected, read-only or unhealthy; quarantine remains"
        );
        anyhow::ensure!(
            self.applied.as_ref().is_some_and(|applied| {
                matches!(&*self.sender.borrow(), Some(SessionFeedResult::Snapshot(latest))
                    if latest.cursor.epoch == applied.cursor.epoch
                        && latest.cursor.revision == applied.cursor.revision)
            }),
            "Current canonical runtime state has not been applied; quarantine remains"
        );
        anyhow::ensure!(
            self.quarantined.remove(id),
            "Session has no indeterminate mutation to resolve"
        );
        Ok(())
    }
    pub(crate) fn namespace_blocked(&self) -> bool {
        self.pending_namespace.is_some() || self.namespace_quarantined
    }

    pub(crate) fn any_pending(&self) -> bool {
        !self.pending.is_empty()
            || !self.bulk_queue.is_empty()
            || !self.pending_creations.is_empty()
            || self.pending_namespace.is_some()
    }

    pub(crate) fn namespace_indeterminate(&self) -> bool {
        self.namespace_quarantined
    }

    pub(crate) fn can_submit_namespace(&self) -> bool {
        self.current_snapshot_applied()
            && !self.namespace_blocked()
            && self.pending.is_empty()
            && self.bulk_queue.is_empty()
            && self.pending_creations.is_empty()
            && self.quarantined.is_empty()
    }

    pub(crate) fn submit_namespace(
        &mut self,
        mutation: crate::daemon::NamespaceMutation,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.can_submit_namespace(), "Namespace change waits for current canonical state and resolution of all pending or indeterminate changes; no change submitted");
        let grant = self.grant.clone();
        let lease = grant.load(Ordering::SeqCst);
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Runtime disconnected"))?;
        let (result, receiver) = tokio::sync::oneshot::channel();
        commands
            .try_send(SessionCommand {
                request: CommandRequest::Namespace(mutation),
                lease,
                native: None,
                result,
            })
            .map_err(|_| {
                anyhow::anyhow!("Runtime command queue unavailable; no change submitted")
            })?;
        self.pending_namespace = Some(PendingNamespace {
            grant,
            lease,
            result: receiver,
            reply: None,
        });
        Ok(())
    }

    pub(crate) fn resolve_namespace_indeterminate(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.current_snapshot_applied(),
            "Current healthy canonical state has not been applied; quarantine remains"
        );
        anyhow::ensure!(
            self.namespace_quarantined,
            "Namespace has no indeterminate mutation to resolve"
        );
        self.namespace_quarantined = false;
        Ok(())
    }

    pub(crate) fn submit_request(
        &mut self,
        id: String,
        request: SessionRequest,
    ) -> anyhow::Result<()> {
        self.enqueue(id, request, None, None)
    }

    pub(crate) fn drain_namespace_result(
        &mut self,
    ) -> Option<Result<MutationReceipt<crate::daemon::NamespaceOutcome>, CommandFailure>> {
        let pending = self.pending_namespace.as_mut()?;
        // A definitive pre-submission refusal remains known even if its lease changed.
        let failure = if pending.reply.is_none() {
            match pending.result.try_recv() {
                Ok(Ok(CommandReply::Namespace(reply))) => {
                    pending.reply = Some(reply);
                    None
                }
                Ok(Ok(_)) => Some(CommandFailure::Unknown(
                    "Unexpected namespace reply; outcome unknown".into(),
                )),
                Ok(Err(error)) => Some(error),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    Some(CommandFailure::Unknown(
                        "Runtime namespace request interrupted; outcome unknown".into(),
                    ))
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            }
        } else {
            None
        };
        let failure = failure.or_else(|| {
            (pending.grant.load(Ordering::SeqCst) != pending.lease).then(|| {
                CommandFailure::Unknown(
                    "Runtime permission changed before namespace confirmation; outcome unknown"
                        .into(),
                )
            })
        });
        if let Some(failure) = failure {
            self.namespace_quarantined = matches!(&failure, CommandFailure::Unknown(_));
            self.pending_namespace = None;
            return Some(Err(failure));
        }
        let cursor = &self.pending_namespace.as_ref()?.reply.as_ref()?.cursor;
        if !self.receipt_applied(cursor) {
            return None;
        }
        Some(Ok(self.pending_namespace.take().unwrap().reply.unwrap()))
    }

    pub(crate) fn drain_session_outcomes(&mut self) -> Vec<(String, SessionCommandOutcome)> {
        std::mem::take(&mut self.row_outcomes)
    }

    pub(crate) fn submit(&mut self, id: String, mutation: SessionMutation) -> anyhow::Result<()> {
        self.enqueue(id, SessionRequest::Mutation(mutation), None, None)
    }
    pub(crate) fn submit_diff_base(
        &mut self,
        id: String,
        base_branch: Option<String>,
    ) -> anyhow::Result<tokio::sync::oneshot::Receiver<Result<RuntimeCursor, CommandFailure>>> {
        self.enqueue(
            id.clone(),
            SessionRequest::Mutation(SessionMutation::DiffBase(
                crate::daemon::UpdateDiffBaseBody {
                    base_branch,
                    repo: None,
                },
            )),
            None,
            None,
        )?;
        let (result, receiver) = tokio::sync::oneshot::channel();
        self.pending
            .get_mut(&id)
            .expect("admitted diff command")
            .diff_save = Some(result);
        Ok(receiver)
    }

    /// Admit the selection before sending any member, then pump as receipts settle.
    pub(crate) fn submit_batch(
        &mut self,
        requests: impl IntoIterator<Item = (String, SessionRequest)>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mutations_available() && !self.namespace_blocked(),
            "Runtime disconnected or unhealthy"
        );
        let requests: Vec<_> = requests.into_iter().collect();
        let mut ids = HashSet::new();
        anyhow::ensure!(
            requests.iter().all(|(id, _)| {
                ids.insert(id.as_str())
                    && !self.quarantined.contains(id)
                    && !self.pending.contains_key(id)
                    && !self.bulk_queue.iter().any(|(queued, _)| queued == id)
            }),
            "Selection contains a session with a pending or indeterminate runtime change"
        );
        self.bulk_queue.extend(requests);
        self.pump_batch();
        Ok(())
    }

    fn pump_batch(&mut self) {
        while self.pending.len() < COMMAND_CAPACITY {
            let Some((id, mutation)) = self.bulk_queue.pop_front() else {
                break;
            };
            if !self.mutations_available() {
                self.bulk_errors.push(SessionCommandError {
                    id,
                    message: "Runtime permission revoked before bulk submission".into(),
                    marks_unread: false,
                    outcome_unknown: false,
                });
                self.bulk_errors
                    .extend(
                        self.bulk_queue
                            .drain(..)
                            .map(|(id, _)| SessionCommandError {
                                id,
                                message: "Runtime permission revoked before bulk submission".into(),
                                marks_unread: false,
                                outcome_unknown: false,
                            }),
                    );
                break;
            }
            if self
                .commands
                .as_ref()
                .is_some_and(|channel| channel.capacity() == 0)
            {
                self.bulk_queue.push_front((id, mutation));
                break;
            }
            if let Err(error) = self.enqueue(id.clone(), mutation, None, None) {
                self.bulk_errors.push(SessionCommandError {
                    id,
                    message: error.to_string(),
                    marks_unread: false,
                    outcome_unknown: false,
                });
            }
        }
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
            lease & 1 == 1 && !self.namespace_blocked(),
            "Runtime disconnected, read-only, unhealthy, or namespace change pending; no change submitted"
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
                request: CommandRequest::Create(Box::new(body)),
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
                receipt: None,
            },
        );
        Ok(())
    }

    /// Request deferred cancellation of a daemon creation. The acknowledgement
    /// is the daemon's, not a promise that the rollback has finished.
    pub(crate) fn cancel_creation(&mut self, id: String) -> anyhow::Result<()> {
        self.enqueue(id, SessionRequest::CancelCreation, None, None)
    }

    /// Await cancellation admission on the original authenticated runtime, not the writer queue.
    pub(crate) async fn cancel_creation_at_quit(
        &mut self,
        request_key: &str,
        daemon_id: Option<&str>,
        origin_epoch: &str,
    ) -> Result<RuntimeCursor, CommandFailure> {
        let control = self
            .quit_control
            .borrow()
            .clone()
            .filter(|control| control.epoch == origin_epoch)
            .ok_or_else(|| {
                CommandFailure::Rejected("Original creation runtime is unavailable".into())
            })?;
        let lease = control.grant.load(Ordering::SeqCst);
        if lease & 1 == 0 {
            return Err(CommandFailure::Rejected(
                "Creation cancellation permission was revoked".into(),
            ));
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut snapshots = self.receiver.clone();
        let mut progresses = self.progress_receiver.clone();
        let cancellation = async {
            let id = loop {
                if control.grant.load(Ordering::SeqCst) != lease {
                    return Err(CommandFailure::Rejected(
                        "Creation cancellation authority changed".into(),
                    ));
                }
                let latest = snapshots.borrow().clone();
                let same_epoch = matches!(&latest, Some(SessionFeedResult::Snapshot(snapshot))
                    if snapshot.cursor.epoch == origin_epoch);
                if !same_epoch {
                    return Err(CommandFailure::Rejected(
                        "Original creation runtime is unavailable".into(),
                    ));
                }
                if let Some(id) = daemon_id {
                    break id.to_owned();
                }
                if let Some(id) = progresses.borrow().as_ref().and_then(|progress| {
                    progress
                        .iter()
                        .find(|entry| entry.request_key.as_deref() == Some(request_key))
                        .map(|entry| entry.session_id.clone())
                }) {
                    break id;
                }
                if let Some(SessionFeedResult::Snapshot(snapshot)) = latest {
                    if let Some(row) = snapshot
                        .contents
                        .sessions
                        .iter()
                        .find(|row| row.idempotency_key.as_deref() == Some(request_key))
                    {
                        break row.id.clone();
                    }
                }
                tokio::select! {
                    changed = snapshots.changed() => changed.map_err(|_| CommandFailure::Unknown("Runtime closed before creation identity resolved".into()))?,
                    changed = progresses.changed() => changed.map_err(|_| CommandFailure::Unknown("Progress closed before creation identity resolved".into()))?,
                }
            };
            if control.grant.load(Ordering::SeqCst) != lease {
                return Err(CommandFailure::Rejected(
                    "Creation cancellation authority changed".into(),
                ));
            }
            control
                .client
                .cancel_creation(&id, origin_epoch)
                .await
                .map_err(CommandFailure::submitted)
        };
        tokio::time::timeout_at(deadline, cancellation)
            .await
            .unwrap_or_else(|_| {
                Err(CommandFailure::Unknown(
                    "Creation cancellation acknowledgement timed out".into(),
                ))
            })
    }

    /// Settled creations, each with the daemon's receipt or its refusal.
    pub(crate) fn drain_creation_results(
        &mut self,
    ) -> Vec<(
        String,
        Result<MutationReceipt<crate::daemon::SessionResponse>, CommandFailure>,
    )> {
        let mut settled = Vec::new();
        let applied = self.applied.as_ref();
        self.pending_creations.retain(|token, pending| {
            if pending.grant.load(Ordering::SeqCst) != pending.lease {
                settled.push((
                    token.clone(),
                    Err(CommandFailure::Unknown(
                        "runtime permission revoked; creation outcome unknown".into(),
                    )),
                ));
                return false;
            }
            if pending.receipt.is_none() {
                match pending.result.try_recv() {
                    Ok(Ok(CommandReply::Created(receipt))) => pending.receipt = Some(receipt),
                    Ok(Ok(_)) => {
                        settled.push((
                            token.clone(),
                            Err(CommandFailure::Unknown("unexpected creation reply".into())),
                        ));
                        return false;
                    }
                    Ok(Err(error)) => {
                        settled.push((token.clone(), Err(error)));
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        settled.push((
                            token.clone(),
                            Err(CommandFailure::Unknown(
                                "runtime request interrupted; creation outcome unknown".into(),
                            )),
                        ));
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return true,
                }
            }
            let receipt = pending.receipt.as_ref().unwrap();
            if applied.is_some_and(|snapshot| {
                snapshot.cursor.epoch == receipt.cursor.epoch
                    && snapshot.cursor.revision >= receipt.cursor.revision
            }) {
                settled.push((token.clone(), Ok(*pending.receipt.take().unwrap())));
                return false;
            }
            true
        });
        settled
    }

    /// Freeze the original role and owner before queueing any native preparation.
    fn preparation_lease(
        &self,
        id: &str,
        auxiliary: Option<AuxiliaryTarget>,
    ) -> anyhow::Result<NativeLease> {
        let generation = self.native_grant.load(Ordering::SeqCst);
        anyhow::ensure!(generation & 1 == 1, "Native interaction is unavailable");
        let snapshot = self
            .applied
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Runtime snapshot is not current"))?;
        let row = snapshot
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| {
                anyhow::anyhow!("Native preparation requires the original session snapshot")
            })?;
        let created_at: chrono::DateTime<chrono::Utc> = row.created_at.parse()?;
        let profile_binding = crate::session::Storage::open_reference(&row.profile)
            .ok()
            .map(Arc::new);
        let pane = auxiliary
            .as_ref()
            .and_then(|target| {
                row.auxiliary
                    .iter()
                    .find(|observation| observation.target == *target)
                    .map(|observation| &observation.pane)
            })
            .or_else(|| auxiliary.is_none().then_some(&row.agent_pane))
            .cloned()
            .unwrap_or_default();
        // A living physical owner needs no selected-context read. If it disappears,
        // this intent must refuse rather than silently prepare a replacement.
        let selection = if auxiliary.is_none() && pane.state != crate::session::PanePresence::Alive
        {
            profile_binding.as_ref().and_then(|storage| {
                let rows = storage.load_instances_complete().ok()?;
                let mut owners = rows.iter().filter(|candidate| candidate.id == row.id);
                let owner = owners.next()?;
                if owners.next().is_some()
                    || owner.created_at != created_at
                    || owner.lifecycle_generation != row.lifecycle_generation
                    || owner.source_profile != row.profile
                {
                    return None;
                }
                owner.native_preparation_selection().ok()
            })
        } else {
            None
        };
        let preparation = Arc::new(NativePreparationScope {
            id: id.to_owned(),
            epoch: snapshot.cursor.epoch.clone(),
            identity: crate::daemon::NativePreparationIdentity {
                created_at: row.created_at.clone(),
                lifecycle_generation: row.lifecycle_generation,
                profile: row.profile.clone(),
                profile_identity: profile_binding
                    .as_ref()
                    .and_then(|storage| storage.bound_profile_identity().ok()),
                selection,
            },
            auxiliary: auxiliary.clone(),
            pane,
            profile_binding,
        });
        let latest = self.receiver.borrow();
        anyhow::ensure!(
            matches!(latest.as_ref(), Some(SessionFeedResult::Snapshot(latest))
                if preparation.matches_initial(latest)),
            "Native preparation scope changed before admission"
        );
        let lease = NativeLease {
            grant: self.native_grant.clone(),
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
            scope: Arc::new(std::sync::OnceLock::new()),
            latest: Some(self.receiver.clone()),
            preparation: Some(preparation.clone()),
        };
        if preparation.pane.state == crate::session::PanePresence::Alive
            && preparation.pane.legacy_tool.is_none()
        {
            if let (Some(name), Some(physical)) =
                (&preparation.pane.tmux_session, &preparation.pane.physical)
            {
                anyhow::ensure!(
                    lease.bind(
                        id,
                        auxiliary,
                        Arc::new(TerminalTarget {
                            tmux_session: name.clone(),
                            status: crate::daemon::TerminalTargetStatus::Alive,
                            lifecycle_generation: row.lifecycle_generation,
                            profile: row.profile.clone(),
                            physical: Some(physical.clone()),
                        }),
                        snapshot,
                        latest.as_ref()
                    ),
                    "Original native target is no longer current"
                );
            }
        }
        Ok(lease)
    }

    pub(crate) fn ensure_agent(
        &mut self,
        id: String,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<NativePreparation> {
        let lease = self.preparation_lease(&id, None)?;
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
                native: lease.clone(),
                result: ready,
            }),
        )?;
        Ok(NativePreparation { lease, result })
    }

    pub(crate) fn has_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    pub(crate) fn has_queued(&self, id: &str) -> bool {
        self.bulk_queue.iter().any(|(queued, _)| queued == id)
    }

    pub(crate) fn ensure_auxiliary(
        &mut self,
        id: String,
        target: AuxiliaryTarget,
        size: Option<(u16, u16)>,
        adoption: Option<crate::session::LegacyToolAdoption>,
    ) -> anyhow::Result<NativePreparation> {
        anyhow::ensure!(
            adoption.is_none() || matches!(&target, AuxiliaryTarget::Tool { .. }),
            "Only a tool can adopt a legacy pane"
        );
        let lease = self.preparation_lease(&id, Some(target.clone()))?;
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
                adoption,
            },
            Some(lease.clone()),
            Some(PendingTerminal {
                target: Some(target),
                native: lease.clone(),
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
            lease & 1 == 1 && grant.load(Ordering::SeqCst) == lease && !self.namespace_blocked(),
            "Runtime disconnected, read-only, unhealthy, or namespace change pending; no change submitted"
        );
        anyhow::ensure!(
            !self.quarantined.contains(&id),
            "The previous runtime change has an unknown outcome; explicitly resolve its quarantine before retrying"
        );
        anyhow::ensure!(
            self.pending.len() < COMMAND_CAPACITY,
            "Runtime command queue is full"
        );
        anyhow::ensure!(
            !self.bulk_queue.iter().any(|(queued, _)| queued == &id),
            "This session already has a queued runtime change"
        );
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Runtime disconnected"))?;
        anyhow::ensure!(
            !commands.is_closed(),
            "Runtime command queue unavailable; no change submitted"
        );
        if let Some(pending) = self.pending.get_mut(&id) {
            anyhow::ensure!(
                pending.is_engagement
                    && pending.deferred_native.is_none()
                    && matches!(
                        &request,
                        SessionRequest::EnsureAgent { .. } | SessionRequest::EnsureAuxiliary { .. }
                    ),
                "This session already has a pending runtime change"
            );
            let native = native
                .ok_or_else(|| anyhow::anyhow!("Native preparation requires original authority"))?;
            let terminal = terminal
                .ok_or_else(|| anyhow::anyhow!("Native preparation requires a continuation"))?;
            anyhow::ensure!(
                native.is_valid() && pending.grant.load(Ordering::SeqCst) == pending.lease,
                "Runtime permission changed before native preparation"
            );
            let snapshot = self
                .applied
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Runtime snapshot is unavailable"))?;
            let row = snapshot
                .contents
                .sessions
                .iter()
                .find(|row| row.id == id)
                .ok_or_else(|| anyhow::anyhow!("Session is unknown to the applied runtime"))?;
            pending.deferred_native = Some(Box::new(DeferredNativePreparation {
                request,
                native,
                terminal,
                epoch: snapshot.cursor.epoch.clone(),
                profile: row.profile.clone(),
                lifecycle_generation: row.lifecycle_generation,
                created_at: row.created_at.clone(),
            }));
            return Ok(());
        }
        let (result, receiver) = tokio::sync::oneshot::channel();
        let marks_unread = matches!(&request, SessionRequest::Mutation(SessionMutation::Unread(body)) if body.unread);
        let is_engagement = matches!(
            &request,
            SessionRequest::Mutation(SessionMutation::Access | SessionMutation::InputRecency(_))
        ) || matches!(&request, SessionRequest::Mutation(SessionMutation::Unread(body)) if !body.unread);
        commands
            .try_send(SessionCommand {
                request: CommandRequest::Session {
                    id: id.clone(),
                    request,
                },
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
                is_engagement,
                deferred_native: None,
                diff_save: None,
            },
        );
        Ok(())
    }

    pub(crate) fn current_snapshot_applied(&self) -> bool {
        self.mutations_available()
            && matches!((self.applied.as_ref(), self.receiver.borrow().as_ref()),
                (Some(applied), Some(SessionFeedResult::Snapshot(latest)))
                    if applied.cursor == latest.cursor)
    }

    pub(crate) fn receipt_applied(&self, cursor: &RuntimeCursor) -> bool {
        self.applied.as_ref().is_some_and(|snapshot| {
            snapshot.cursor.epoch == cursor.epoch && snapshot.cursor.revision >= cursor.revision
        })
    }

    pub(crate) fn applied_session(&self, id: &str) -> Option<&crate::daemon::SessionResponse> {
        self.applied
            .as_ref()?
            .contents
            .sessions
            .iter()
            .find(|row| row.id == id)
    }

    pub(crate) fn applied_epoch(&self) -> Option<&str> {
        self.applied
            .as_ref()
            .map(|snapshot| snapshot.cursor.epoch.as_str())
    }

    pub(crate) fn applied_snapshot(&self) -> Option<Arc<RuntimeSnapshot>> {
        self.applied.clone()
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
        let mut unknown_ids = Vec::new();
        let latest_receiver = self.receiver.clone();
        let latest = latest_receiver.borrow();
        let namespace_blocked = self.namespace_blocked();
        let native_available = self.native_interaction_available();
        let mut deferred = Vec::new();
        let outcomes = &mut self.row_outcomes;
        let quarantined = &self.quarantined;
        self.pending.retain(|id, pending| {
            if pending.deferred_native.as_ref().is_some_and(|child| {
                !child.native.is_valid() || !native_available || namespace_blocked || quarantined.contains(id)
                    || snapshot.is_none_or(|snapshot| !child.matches(id, snapshot))
                    || !matches!(latest.as_ref(), Some(SessionFeedResult::Snapshot(snapshot)) if child.matches(id, snapshot))
            }) {
                pending.deferred_native.take().unwrap().reject("Native preparation authority or session identity changed");
            }
            if pending.reply.is_none() {
                match pending.result.try_recv() {
                    Ok(Ok(reply)) => pending.reply = Some(reply),
                    Ok(Err(error)) => {
                        if matches!(&error, CommandFailure::Unknown(_)) {
                            unknown_ids.push(id.clone());
                        }
                        pending.fail(id, error, &mut errors);
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        unknown_ids.push(id.clone());
                        pending.fail(
                            id,
                            CommandFailure::Unknown("runtime request interrupted; outcome unknown".into()),
                            &mut errors,
                        );
                        return false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return true,
                }
            }
            if pending.grant.load(Ordering::SeqCst) != pending.lease {
                unknown_ids.push(id.clone());
                pending.fail(
                    id,
                    CommandFailure::Unknown("runtime permission changed before confirmation; outcome unknown".into()),
                    &mut errors,
                );
                return false;
            }
            if let (Some(reply), Some(snapshot)) = (&pending.reply, snapshot) {
                let cursor = reply.cursor();
                if cursor.epoch == snapshot.cursor.epoch
                    && cursor.revision <= snapshot.cursor.revision
                {
                    if matches!(reply, CommandReply::CommittedFailure { .. }) {
                        let Some(CommandReply::CommittedFailure { failure, .. }) = pending.reply.take() else {
                            unreachable!();
                        };
                        if matches!(&failure, CommandFailure::Unknown(_)) {
                            unknown_ids.push(id.clone());
                        }
                        pending.fail(id, failure, &mut errors);
                        return false;
                    }
                    if pending.deferred_native.is_some()
                        && !matches!(latest.as_ref(), Some(SessionFeedResult::Snapshot(latest)) if latest.cursor == snapshot.cursor)
                    {
                        return true;
                    }
                    if let Some(terminal) = pending.terminal.take() {
                        let PendingTerminal { target, native, result } = terminal;
                        let prepared = match pending.reply.take() {
                            Some(CommandReply::Terminal(receipt)) => Some(receipt.outcome),
                            _ => None,
                        }.map(Arc::new);
                        match prepared {
                            Some(prepared) if native.bind(id, target, prepared.clone(),
                                snapshot, latest.as_ref()) => {
                                let _ = result.send(Ok(native.target().expect("bound native scope").clone()));
                            }
                            _ => {
                                native.cancel_continuation();
                                let _ = result.send(Err(
                                    "Prepared target no longer matches the current physical snapshot".into(),
                                ));
                            }
                        }
                    }
                    if let Some(reply) = pending.reply.take() {
                        if let Some(result) = pending.diff_save.take() {
                            let _ = result.send(Ok(reply.cursor().clone()));
                        }
                        let outcome = match reply {
                            CommandReply::Trash(receipt) => Some(SessionCommandOutcome::Trashed(receipt)),
                            CommandReply::Purge(receipt) => Some(SessionCommandOutcome::Purged(receipt)),
                            CommandReply::Restore(receipt) => Some(SessionCommandOutcome::Restored(receipt)),
                            CommandReply::Rename(receipt) => Some(SessionCommandOutcome::Renamed(receipt)),
                            CommandReply::WorktreeName(receipt) => Some(SessionCommandOutcome::WorktreeEdited(receipt)),
                            CommandReply::AttachProject(receipt) => Some(SessionCommandOutcome::ProjectAttached(receipt)),
                            _ => None,
                        };
                        if let Some(outcome) = outcome { outcomes.push((id.clone(), outcome)); }
                    }
                    if let Some(child) = pending.deferred_native.take() {
                        deferred.push((id.clone(), child));
                    }
                    return false;
                }
            }
            true
        });
        self.quarantined.extend(unknown_ids);
        for (id, child) in deferred {
            if !child.native.is_valid()
                || !self.native_interaction_available()
                || self
                    .applied
                    .as_ref()
                    .is_none_or(|snapshot| !child.matches(&id, snapshot))
            {
                child.reject("Native preparation authority or session identity changed");
                continue;
            }
            let DeferredNativePreparation {
                request,
                native,
                terminal,
                ..
            } = *child;
            // A refused enqueue drops its readiness sender; the existing native
            // continuation reports that closure rather than treating it as success.
            let _ = self.enqueue(id, request, Some(native), Some(terminal));
        }
        drop(latest);
        self.pump_batch();
        errors.append(&mut self.bulk_errors);
        errors
    }

    pub(crate) fn try_recv(&mut self) -> Result<SessionFeedResult, TryRecvError> {
        if self.receiver.has_changed().unwrap_or(false) {
            return self
                .receiver
                .borrow_and_update()
                .clone()
                .ok_or(TryRecvError::Empty);
        }
        let cached = self.receiver.borrow();
        match cached.as_ref() {
            Some(SessionFeedResult::Snapshot(snapshot))
                if self
                    .applied
                    .as_ref()
                    .is_none_or(|applied| applied.cursor != snapshot.cursor) =>
            {
                Ok(SessionFeedResult::Snapshot(snapshot.clone()))
            }
            _ => Err(TryRecvError::Empty),
        }
    }

    #[cfg(test)]
    pub(crate) fn engagement_native_driver_for_test(
        &mut self,
    ) -> impl FnMut(Result<RuntimeCursor, String>, Option<TerminalTarget>) -> Option<SessionRequest>
    {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |cursor, target| {
            let command = receiver.try_recv().ok()?;
            let CommandRequest::Session { request, .. } = command.request else {
                panic!("expected a row command");
            };
            assert_eq!(
                target.is_some(),
                matches!(
                    &request,
                    SessionRequest::EnsureAgent { .. } | SessionRequest::EnsureAuxiliary { .. }
                )
            );
            let reply = cursor
                .map(|cursor| match target {
                    Some(outcome) => CommandReply::Terminal(MutationReceipt { cursor, outcome }),
                    None => CommandReply::Mutation(cursor),
                })
                .map_err(CommandFailure::Rejected);
            assert!(command.result.send(reply).is_ok());
            Some(request)
        }
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
                CommandRequest::Session {
                    request: SessionRequest::EnsureAgent { .. }
                        | SessionRequest::EnsureAuxiliary { .. },
                    ..
                }
            ));
            assert!(command
                .result
                .send(
                    result
                        .map(CommandReply::Terminal)
                        .map_err(CommandFailure::Rejected)
                )
                .is_ok());
        }
    }

    #[cfg(test)]
    pub(crate) fn legacy_tool_driver_for_test(
        &mut self,
    ) -> impl FnMut(&crate::session::LegacyToolAdoption) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |expected| {
            let command = receiver.try_recv().expect("confirmed native preparation");
            let CommandRequest::Session {
                request: SessionRequest::EnsureAuxiliary { adoption, .. },
                ..
            } = command.request
            else {
                panic!("wrong preparation request");
            };
            assert_eq!(adoption.as_ref(), Some(expected));
            assert!(command
                .result
                .send(Err(CommandFailure::Rejected(
                    "CAS refused stale legacy identity".into()
                )))
                .is_ok());
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
                    CommandRequest::Create(body) => seen.push(format!(
                        "create:{}",
                        body.idempotency_key.as_deref().unwrap_or_default()
                    )),
                    CommandRequest::Session {
                        id,
                        request: SessionRequest::CancelCreation,
                    } => {
                        seen.push(format!("cancel:{}", id));
                        let _ = command
                            .result
                            .send(Ok(CommandReply::Mutation(RuntimeCursor {
                                epoch: "test".into(),
                                revision: 1,
                            })));
                    }
                    _ => {}
                }
            }
            seen
        }
    }

    #[cfg(test)]
    pub(crate) fn creation_receipt_driver_for_test(
        &mut self,
    ) -> impl FnMut(MutationReceipt<crate::daemon::SessionResponse>) -> String {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |receipt| {
            let command = receiver.try_recv().expect("creation queued");
            let CommandRequest::Create(body) = command.request else {
                panic!("expected creation");
            };
            let key = body.idempotency_key.expect("creation idempotency identity");
            assert!(command
                .result
                .send(Ok(CommandReply::Created(Box::new(receipt))))
                .is_ok());
            key
        }
    }

    #[cfg(test)]
    pub(crate) fn creation_rejection_driver_for_test(&mut self) -> impl FnMut() -> String {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        move || {
            let command = receiver.try_recv().expect("creation queued");
            let CommandRequest::Create(body) = command.request else {
                panic!("expected creation");
            };
            let key = body.idempotency_key.expect("creation idempotency identity");
            assert!(command
                .result
                .send(Err(CommandFailure::Rejected("denied".into())))
                .is_ok());
            key
        }
    }

    #[cfg(test)]
    pub(crate) fn publish_progress_for_test(&self, progress: Vec<CreationProgress>) {
        self.progress.send_replace(Some(progress));
    }

    #[cfg(test)]
    pub(crate) fn command_writer_for_test(
        &mut self,
        client: crate::daemon::DaemonClient,
        epoch: String,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (sender, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        run_command_writer(client, epoch, self.grant.clone(), receiver, None)
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
                .send(
                    result
                        .map(CommandReply::Mutation)
                        .map_err(CommandFailure::Rejected)
                )
                .is_ok());
            let CommandRequest::Session {
                id,
                request: SessionRequest::Mutation(mutation),
            } = command.request
            else {
                panic!("expected session mutation");
            };
            Some((id, mutation))
        }
    }

    #[cfg(test)]
    pub(crate) fn command_dispatch_driver_for_test<F>(
        &mut self,
        mut dispatch: F,
    ) -> impl FnMut() -> Option<String>
    where
        F: FnMut(&str, &SessionMutation) -> Result<RuntimeCursor, String>,
    {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        let grant = self.grant.clone();
        move || {
            let command = receiver.try_recv().ok()?;
            let allowed = command.native.as_ref().map_or_else(
                || grant.load(Ordering::SeqCst) == command.lease,
                NativeLease::is_valid,
            );
            let CommandRequest::Session {
                id,
                request: SessionRequest::Mutation(mutation),
            } = command.request
            else {
                panic!("expected session mutation");
            };
            let result = if allowed {
                dispatch(&id, &mutation)
            } else {
                Err("Request not submitted: runtime permission changed or continuation was cancelled".into())
            };
            assert!(command
                .result
                .send(
                    result
                        .map(CommandReply::Mutation)
                        .map_err(CommandFailure::Rejected)
                )
                .is_ok());
            Some(id)
        }
    }

    #[cfg(test)]
    pub(crate) fn request_driver_for_test(
        &mut self,
    ) -> impl FnMut(Result<SessionCommandOutcome, String>) -> Option<(String, SessionRequest)> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |result| {
            let command = receiver.try_recv().ok()?;
            let CommandRequest::Session { id, request } = command.request else {
                panic!("expected a row command");
            };
            let reply = result
                .map(|outcome| match outcome {
                    SessionCommandOutcome::Trashed(receipt) => CommandReply::Trash(receipt),
                    SessionCommandOutcome::Purged(receipt) => CommandReply::Purge(receipt),
                    SessionCommandOutcome::Restored(receipt) => CommandReply::Restore(receipt),
                    SessionCommandOutcome::Renamed(receipt) => CommandReply::Rename(receipt),
                    SessionCommandOutcome::WorktreeEdited(receipt) => {
                        CommandReply::WorktreeName(receipt)
                    }
                    SessionCommandOutcome::ProjectAttached(receipt) => {
                        CommandReply::AttachProject(receipt)
                    }
                })
                .map_err(CommandFailure::Rejected);
            assert!(command.result.send(reply).is_ok());
            Some((id, request))
        }
    }

    #[cfg(test)]
    pub(crate) fn namespace_driver_for_test(
        &mut self,
    ) -> impl FnMut(
        Result<MutationReceipt<crate::daemon::NamespaceOutcome>, String>,
    ) -> Option<crate::daemon::NamespaceMutation> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SessionCommand>(COMMAND_CAPACITY);
        self.commands = Some(sender);
        set_grant(&self.grant, true);
        set_grant(&self.native_grant, true);
        move |result| {
            let command = receiver.try_recv().ok()?;
            let CommandRequest::Namespace(request) = command.request else {
                panic!("expected a namespace command");
            };
            assert!(command
                .result
                .send(
                    result
                        .map(CommandReply::Namespace)
                        .map_err(CommandFailure::Rejected)
                )
                .is_ok());
            Some(request)
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

    #[cfg(test)]
    pub(crate) fn next_revision_for_test(&self) -> u64 {
        self.applied
            .as_ref()
            .map_or(2, |snapshot| snapshot.cursor.revision + 1)
    }

    #[cfg(test)]
    pub(crate) fn set_mutation_permission_for_test(&self, allowed: bool) {
        set_grant(&self.grant, allowed);
    }

    #[cfg(test)]
    pub(crate) fn set_native_permission_for_test(&self, allowed: bool) {
        set_grant(&self.native_grant, allowed);
    }

    #[cfg(test)]
    pub(crate) fn set_cityhall_for_test(&self, cityhall: bool) {
        self.cityhall.store(cityhall, Ordering::SeqCst);
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

    #[test]
    fn create_transport_timeout_is_not_reported_as_refusal() {
        use crate::daemon::DaemonClientError;
        for error in [
            DaemonClientError::Timeout,
            DaemonClientError::Transport,
            DaemonClientError::Status {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                code: None,
                body: String::new(),
                truncated: false,
            },
            DaemonClientError::Status {
                status: reqwest::StatusCode::REQUEST_TIMEOUT,
                code: None,
                body: String::new(),
                truncated: false,
            },
        ] {
            assert!(matches!(
                CommandFailure::creation(error),
                CommandFailure::Unknown(_)
            ));
        }
        assert!(matches!(
            CommandFailure::creation(DaemonClientError::Status {
                status: reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                code: None,
                body: String::new(),
                truncated: false,
            }),
            CommandFailure::Rejected(_),
        ));
    }

    #[test]
    fn submitted_mutations_only_report_definitive_4xx_as_rejected() {
        use crate::daemon::DaemonClientError;
        for error in [
            DaemonClientError::Timeout,
            DaemonClientError::Transport,
            DaemonClientError::Status {
                status: reqwest::StatusCode::REQUEST_TIMEOUT,
                code: None,
                body: String::new(),
                truncated: false,
            },
            DaemonClientError::Status {
                status: reqwest::StatusCode::BAD_GATEWAY,
                code: None,
                body: String::new(),
                truncated: false,
            },
            DaemonClientError::ResponseTooLarge { limit: 10 },
            DaemonClientError::InvalidMutationReceipt,
        ] {
            assert!(matches!(
                CommandFailure::submitted(error),
                CommandFailure::Unknown(_)
            ));
        }
        assert!(matches!(
            CommandFailure::submitted(DaemonClientError::Status {
                status: reqwest::StatusCode::CONFLICT,
                code: None,
                body: String::new(),
                truncated: false,
            }),
            CommandFailure::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn unknown_mutation_quarantine_survives_reconnect_but_refusal_releases_fence() {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        feed.mark_snapshot_applied(snapshot("test", 1));
        let (commands, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(commands);
        set_grant(&feed.grant, true);

        feed.submit("unknown".into(), SessionMutation::Stop)
            .unwrap();
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Err(CommandFailure::Unknown("timeout".into())))
            .unwrap();
        let errors = feed.drain_command_errors();
        assert!(errors[0].outcome_unknown);
        assert!(!feed.can_submit("unknown"));
        assert!(feed.can_submit("refused"));

        feed.submit("refused".into(), SessionMutation::Stop)
            .unwrap();
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Err(CommandFailure::Rejected("conflict".into())))
            .unwrap();
        let errors = feed.drain_command_errors();
        assert!(!errors[0].outcome_unknown);
        assert!(feed.can_submit("refused"));

        feed.connect("test".into());
        feed.task.take().unwrap().abort();
        set_grant(&feed.grant, true);
        assert!(!feed.can_submit("unknown"));
        feed.publish_for_test(SessionFeedResult::Snapshot(snapshot("reconnected", 2)));
        assert!(feed.resolve_indeterminate("unknown").is_err());
        feed.mark_snapshot_applied(snapshot("reconnected", 2));
        assert!(!feed.can_submit("unknown"));
        feed.resolve_indeterminate("unknown").unwrap();
        let mut driver = feed.command_driver_for_test();
        feed.submit("unknown".into(), SessionMutation::Stop)
            .unwrap();
        let (id, mutation) = driver(Ok(RuntimeCursor {
            epoch: "reconnected".into(),
            revision: 3,
        }))
        .expect("mutation submitted after explicit resolution");
        assert_eq!(id, "unknown");
        assert!(matches!(mutation, SessionMutation::Stop));
    }

    #[tokio::test]
    async fn committed_failure_fences_until_publication_and_only_unknown_outcomes_quarantine() {
        use axum::response::IntoResponse;
        for (request, code, known) in [
            (
                SessionRequest::Mutation(SessionMutation::Start(StartSessionBody::default())),
                "start_failed",
                false,
            ),
            (
                SessionRequest::Purge(crate::daemon::DeleteSessionBody::default()),
                "purge_failed",
                true,
            ),
            (
                SessionRequest::Mutation(SessionMutation::Start(StartSessionBody::default())),
                "managed_resume_refused",
                true,
            ),
        ] {
            let app = axum::Router::new().fallback(move || async move {
                (
                    if code == "start_failed" {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        axum::http::StatusCode::CONFLICT
                    },
                    [
                        (crate::daemon::ERROR_CODE_HEADER, code),
                        (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                        (crate::daemon::RUNTIME_REVISION_HEADER, "2"),
                    ],
                    "private refusal detail",
                )
                    .into_response()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = crate::daemon::DaemonClient::new(
                &format!("http://{}", listener.local_addr().unwrap()),
                Some("private-token"),
            )
            .unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (mut feed, requests) = feed_with_native_lane();
            let writer = tokio::spawn(run_command_writer(
                client,
                "test".into(),
                feed.grant.clone(),
                requests,
                None,
            ));
            feed.submit_request("partial".into(), request).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    assert!(feed.drain_command_errors().is_empty());
                    if feed.pending["partial"].reply.is_some() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("writer receives committed refusal");
            assert!(feed.drain_command_errors().is_empty());
            assert!(!feed.can_submit("partial"));
            let next = snapshot("test", 2);
            feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
            feed.mark_snapshot_applied(next);
            let errors = feed.drain_command_errors();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].outcome_unknown, !known);
            assert_eq!(feed.can_submit("partial"), known);
            if !known {
                feed.resolve_indeterminate("partial").unwrap();
                assert!(feed.can_submit("partial"));
            }
            drop(feed.commands.take());
            writer.await.unwrap();
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn auto_name_ack_only_settles_after_its_canonical_revision() {
        let (mut feed, requests) = feed_with_native_lane();
        let mut instance = crate::session::Instance::new("original-title", "/tmp/rename");
        instance.id = "rename".into();
        instance.source_profile = "test".into();
        let row = crate::daemon::SessionResponse::from_instance(&instance, false);
        let mut original = snapshot("test", 1);
        Arc::make_mut(&mut original)
            .contents
            .sessions
            .push(row.clone());
        feed.publish_for_test(SessionFeedResult::Snapshot(original.clone()));
        feed.mark_snapshot_applied(original);

        let mut response_row = row.clone();
        response_row.title = "receipt-only-title".into();
        let app = axum::Router::new().route(
            "/api/sessions/{id}/smart-rename",
            axum::routing::post(move |request: axum::extract::Request| {
                let response_row = response_row.clone();
                async move {
                    assert_eq!(
                        request.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
                        "test"
                    );
                    (
                        [
                            (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                            (crate::daemon::RUNTIME_REVISION_HEADER, "2"),
                        ],
                        axum::Json(response_row),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::daemon::DaemonClient::new(&format!("http://{address}"), None).unwrap();
        let writer = tokio::spawn(run_command_writer(
            client,
            "test".into(),
            feed.grant.clone(),
            requests,
            None,
        ));
        feed.submit_request("rename".into(), SessionRequest::SmartRename)
            .unwrap();
        assert!(!feed.can_submit("rename"));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                assert!(feed.drain_command_errors().is_empty());
                if feed.pending["rename"].reply.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the native writer receives the HTTP acknowledgment");
        assert!(
            !feed.can_submit("rename"),
            "an ACK alone cannot release the row fence"
        );
        assert_eq!(
            feed.applied_session("rename").unwrap().title,
            "original-title"
        );

        let mut canonical_row = row;
        canonical_row.title = "canonical-title".into();
        let mut next = snapshot("test", 2);
        Arc::make_mut(&mut next)
            .contents
            .sessions
            .push(canonical_row);
        feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
        feed.mark_snapshot_applied(next);
        assert!(feed.drain_command_errors().is_empty());
        assert!(feed.can_submit("rename"));
        assert_eq!(
            feed.applied_session("rename").unwrap().title,
            "canonical-title"
        );
        drop(feed.commands.take());
        writer.await.unwrap();
        server.abort();
        let _ = server.await;
    }

    /// A feed with a live command lane, both grants open, and one revision
    /// already applied: what a native command needs to be admitted.
    fn feed_with_native_lane() -> (SessionFeed, tokio::sync::mpsc::Receiver<SessionCommand>) {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        feed.mark_snapshot_applied(snapshot("test", 1));
        let (commands, requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(commands);
        set_grant(&feed.grant, true);
        set_grant(&feed.native_grant, true);
        (feed, requests)
    }

    fn native_physical_fixture() -> Arc<crate::session::LegacyToolObservation> {
        let pid = std::process::id();
        Arc::new(crate::session::LegacyToolObservation {
            identity: crate::session::LegacyToolIdentity {
                session_id: "$1".into(),
                pane_id: "%1".into(),
                pane_pid: Some(pid),
            },
            server: Arc::new(crate::session::TmuxServerIdentity {
                socket: "/fixture-tmux.sock".into(),
                pid,
                birth: crate::process::process_birth_identity(pid)
                    .unwrap()
                    .unwrap(),
            }),
        })
    }

    fn native_identity_snapshot(revision: u64) -> Arc<RuntimeSnapshot> {
        let mut snapshot = snapshot("test", revision);
        Arc::make_mut(&mut snapshot).contents.sessions.push(serde_json::from_value(serde_json::json!({
            "id": "session", "profile": "test", "lifecycle_generation": 4,
            "created_at": "2026-01-01T00:00:00Z", "status": "Idle",
            "agent_pane": {"state": "alive", "tmux_session": "native-owned", "physical": native_physical_fixture()},
            "auxiliary": [{"target": {"kind": "host", "index": 0}, "state": "alive", "tmux_session": "native-owned", "physical": native_physical_fixture()}],
        })).unwrap());
        snapshot
    }

    fn feed_with_native_identity(
        id: &str,
    ) -> (SessionFeed, tokio::sync::mpsc::Receiver<SessionCommand>) {
        let (mut feed, requests) = feed_with_native_lane();
        let mut initial = native_identity_snapshot(1);
        Arc::make_mut(&mut initial).contents.sessions[0].id = id.to_owned();
        feed.publish_for_test(SessionFeedResult::Snapshot(initial.clone()));
        feed.mark_snapshot_applied(initial);
        (feed, requests)
    }

    #[test]
    fn native_ensure_waits_for_engagement_and_current_applied_authority() {
        for unread in [false, true] {
            for agent in [false, true] {
                let (mut feed, mut requests) = feed_with_native_lane();
                let initial = native_identity_snapshot(1);
                feed.publish_for_test(SessionFeedResult::Snapshot(initial.clone()));
                feed.mark_snapshot_applied(initial);
                let engagement = if unread {
                    SessionMutation::Unread(crate::daemon::UpdateUnreadBody { unread: false })
                } else {
                    SessionMutation::Access
                };
                feed.submit("session".into(), engagement).unwrap();
                let parent = requests.try_recv().unwrap();
                let mut prepared = if agent {
                    feed.ensure_agent("session".into(), Some((80, 24)))
                } else {
                    feed.ensure_auxiliary(
                        "session".into(),
                        AuxiliaryTarget::Host { index: 0 },
                        Some((80, 24)),
                        None,
                    )
                }
                .unwrap();
                assert!(feed.ensure_agent("session".into(), None).is_err());
                assert!(feed
                    .submit("session".into(), SessionMutation::Stop)
                    .is_err());
                assert!(
                    requests.try_recv().is_err(),
                    "native transport must wait behind engagement"
                );
                assert!(prepared.result.try_recv().is_err());
                parent
                    .result
                    .send(Ok(CommandReply::Mutation(RuntimeCursor {
                        epoch: "test".into(),
                        revision: 2,
                    })))
                    .unwrap();
                assert!(feed.drain_command_errors().is_empty());
                assert!(requests.try_recv().is_err());
                let committed = native_identity_snapshot(2);
                feed.publish_for_test(SessionFeedResult::Snapshot(committed.clone()));
                assert!(feed.drain_command_errors().is_empty());
                assert!(
                    requests.try_recv().is_err(),
                    "receipt alone is not canonical application"
                );
                let latest = native_identity_snapshot(3);
                feed.publish_for_test(SessionFeedResult::Snapshot(latest.clone()));
                feed.mark_snapshot_applied(committed);
                for _ in 0..2 {
                    assert!(feed.drain_command_errors().is_empty());
                    assert!(
                        requests.try_recv().is_err(),
                        "latest snapshot still awaits application"
                    );
                    assert!(prepared.result.try_recv().is_err());
                }
                feed.mark_snapshot_applied(latest);
                assert!(feed.drain_command_errors().is_empty());
                let native = requests
                    .try_recv()
                    .expect("one original native ensure must be promoted");
                assert!(
                    matches!(
                        &native.request,
                        CommandRequest::Session {
                            request: SessionRequest::EnsureAgent { .. },
                            ..
                        }
                    ) == agent
                );
                assert!(native.native.as_ref().unwrap().is_valid());
                assert!(requests.try_recv().is_err());
                assert!(prepared.result.try_recv().is_err());
                native
                    .result
                    .send(Ok(CommandReply::Terminal(MutationReceipt {
                        cursor: RuntimeCursor {
                            epoch: "test".into(),
                            revision: 4,
                        },
                        outcome: TerminalTarget {
                            physical: Some(native_physical_fixture()),
                            tmux_session: "native-owned".into(),
                            status: crate::daemon::TerminalTargetStatus::Exists,
                            lifecycle_generation: 4,
                            profile: "test".into(),
                        },
                    })))
                    .unwrap();
                assert!(feed.drain_command_errors().is_empty());
                assert!(prepared.result.try_recv().is_err());
                let ready = native_identity_snapshot(4);
                feed.publish_for_test(SessionFeedResult::Snapshot(ready.clone()));
                feed.mark_snapshot_applied(ready);
                assert!(feed.drain_command_errors().is_empty());
                assert_eq!(
                    prepared.result.try_recv().unwrap().unwrap().tmux_session,
                    "native-owned"
                );
                assert!(requests.try_recv().is_err());
                assert!(feed.can_submit("session"));
            }
        }
    }

    #[test]
    fn deferred_native_ensure_never_replays_after_failure_or_authority_loss() {
        for change in [
            "rejected",
            "unknown",
            "closed",
            "committed-failure",
            "cancel",
            "permission",
            "epoch",
            "namespace",
            "profile",
            "generation",
            "birth",
            "deleted",
        ] {
            let (mut feed, mut requests) = feed_with_native_lane();
            let initial = native_identity_snapshot(1);
            feed.publish_for_test(SessionFeedResult::Snapshot(initial.clone()));
            feed.mark_snapshot_applied(initial);
            feed.submit("session".into(), SessionMutation::Access)
                .unwrap();
            let parent = requests.try_recv().unwrap();
            let mut prepared = feed
                .ensure_auxiliary(
                    "session".into(),
                    AuxiliaryTarget::Host { index: 0 },
                    None,
                    None,
                )
                .unwrap();
            let mut next = native_identity_snapshot(2);
            match change {
                "cancel" => prepared.lease.cancel_continuation(),
                "permission" => {
                    set_grant(&feed.native_grant, false);
                    set_grant(&feed.native_grant, true);
                }
                "epoch" => {
                    Arc::make_mut(&mut next).cursor.epoch = "replacement".into();
                    set_grant(&feed.grant, false);
                    set_grant(&feed.grant, true);
                    set_grant(&feed.native_grant, false);
                    set_grant(&feed.native_grant, true);
                }
                "namespace" => feed.namespace_quarantined = true,
                "profile" => {
                    Arc::make_mut(&mut next).contents.sessions[0].profile = "replacement".into()
                }
                "generation" => {
                    Arc::make_mut(&mut next).contents.sessions[0].lifecycle_generation += 1
                }
                "birth" => {
                    Arc::make_mut(&mut next).contents.sessions[0].created_at =
                        "2026-02-01T00:00:00Z".into()
                }
                "deleted" => Arc::make_mut(&mut next).contents.sessions.clear(),
                _ => {}
            }
            if change == "closed" {
                drop(parent);
            } else {
                let cursor = RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                };
                let reply = match change {
                    "rejected" => Err(CommandFailure::Rejected("refused".into())),
                    "unknown" => Err(CommandFailure::Unknown("transport lost".into())),
                    "committed-failure" => Ok(CommandReply::CommittedFailure {
                        cursor,
                        failure: CommandFailure::Rejected("committed failure".into()),
                    }),
                    _ => Ok(CommandReply::Mutation(cursor)),
                };
                parent.result.send(reply).unwrap();
            }
            feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
            feed.mark_snapshot_applied(next);
            feed.drain_command_errors();
            assert!(matches!(prepared.result.try_recv(), Ok(Err(_))), "{change}");
            assert!(
                requests.try_recv().is_err(),
                "{change}: no native request may be sent"
            );
            if matches!(change, "unknown" | "closed" | "epoch") {
                assert!(feed.quarantined.contains("session"));
            }
            feed.namespace_quarantined = false;
            let restored = native_identity_snapshot(3);
            feed.publish_for_test(SessionFeedResult::Snapshot(restored.clone()));
            feed.mark_snapshot_applied(restored);
            feed.drain_command_errors();
            assert!(
                requests.try_recv().is_err(),
                "{change}: discarded work must not replay"
            );
        }
    }

    #[test]
    fn native_deferral_does_not_bypass_other_pending_or_queued_commands() {
        for parent in [
            SessionMutation::Stop,
            SessionMutation::Unread(crate::daemon::UpdateUnreadBody { unread: true }),
        ] {
            let (mut feed, mut requests) = feed_with_native_lane();
            let current = native_identity_snapshot(1);
            feed.publish_for_test(SessionFeedResult::Snapshot(current.clone()));
            feed.mark_snapshot_applied(current);
            feed.submit("session".into(), parent).unwrap();
            let _parent = requests.try_recv().unwrap();
            assert!(feed.ensure_agent("session".into(), None).is_err());
            assert!(requests.try_recv().is_err());
        }
        for parent in 0..2 {
            let (mut feed, mut requests) = feed_with_native_lane();
            let current = native_identity_snapshot(1);
            feed.publish_for_test(SessionFeedResult::Snapshot(current.clone()));
            feed.mark_snapshot_applied(current);
            let _preparation = match parent {
                0 => feed.ensure_agent("session".into(), None),
                _ => feed.ensure_auxiliary(
                    "session".into(),
                    AuxiliaryTarget::Host { index: 0 },
                    None,
                    None,
                ),
            }
            .unwrap();
            let _parent = requests.try_recv().unwrap();
            assert!(feed
                .ensure_auxiliary(
                    "session".into(),
                    AuxiliaryTarget::Host { index: 0 },
                    None,
                    None
                )
                .is_err());
            assert!(requests.try_recv().is_err());
        }
        let (mut feed, mut requests) = feed_with_native_lane();
        let current = native_identity_snapshot(1);
        feed.publish_for_test(SessionFeedResult::Snapshot(current.clone()));
        feed.mark_snapshot_applied(current);
        feed.submit("session".into(), SessionMutation::Access)
            .unwrap();
        let _parent = requests.try_recv().unwrap();
        feed.bulk_queue.push_back((
            "session".into(),
            SessionRequest::Mutation(SessionMutation::Stop),
        ));
        assert!(feed.ensure_agent("session".into(), None).is_err());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn dropping_the_feed_rejects_its_deferred_native_child() {
        let (mut feed, mut requests) = feed_with_native_lane();
        let current = native_identity_snapshot(1);
        feed.publish_for_test(SessionFeedResult::Snapshot(current.clone()));
        feed.mark_snapshot_applied(current);
        feed.submit("session".into(), SessionMutation::Access)
            .unwrap();
        let _parent = requests.try_recv().unwrap();
        let mut prepared = feed.ensure_agent("session".into(), None).unwrap();
        drop(feed);
        assert!(matches!(
            prepared.result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(requests.try_recv().is_err());
    }

    /// A native command carries a terminal, so its failure is reported to the
    /// attachment. An unknown outcome is a second fact: the row is quarantined
    /// until an operator resolves it, and the queued error is the only path to
    /// that prompt. Answering the terminal must not swallow it, or the row
    /// stays blocked with nothing to unblock it.
    #[tokio::test]
    async fn unknown_native_outcome_still_queues_the_resolution_error() {
        use crate::daemon::DaemonClientError;
        let (mut feed, mut requests) = feed_with_native_identity("native");
        let mut preparation = feed
            .ensure_agent("native".into(), None)
            .expect("the native command is admitted");
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Err(CommandFailure::submitted(DaemonClientError::Timeout)))
            .unwrap();

        let errors = feed.drain_command_errors();

        assert_eq!(errors.len(), 1, "the unknown outcome must reach the queue");
        assert_eq!(
            errors[0].id, "native",
            "the resolution prompt names the row that is blocked"
        );
        assert!(
            errors[0].outcome_unknown,
            "the queued error is the one that opens the resolution prompt"
        );
        assert!(
            !feed.can_submit("native"),
            "the row stays blocked until the operator resolves it"
        );
        assert!(
            preparation
                .result
                .try_recv()
                .expect("the terminal waiter is answered too")
                .is_err(),
            "the attachment must not wait on a terminal that already failed"
        );
    }

    /// The other side of the same branch: a definitive refusal has no unknown
    /// outcome, so the terminal answer is the whole report. Queueing it too
    /// would put a resolution prompt in front of an operator for a change the
    /// runtime already rejected, and block a row nothing is waiting on.
    #[tokio::test]
    async fn a_refused_native_command_is_not_queued() {
        use crate::daemon::DaemonClientError;
        let (mut feed, mut requests) = feed_with_native_identity("native");
        let mut preparation = feed
            .ensure_agent("native".into(), None)
            .expect("the native command is admitted");
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Err(CommandFailure::submitted(DaemonClientError::Status {
                status: reqwest::StatusCode::CONFLICT,
                code: None,
                body: String::new(),
                truncated: false,
            })))
            .unwrap();

        let errors = feed.drain_command_errors();

        assert!(errors.is_empty(), "a refusal is not an unknown outcome");
        assert!(
            feed.can_submit("native"),
            "a refused change leaves the row usable"
        );
        assert!(
            preparation
                .result
                .try_recv()
                .expect("the terminal waiter is answered")
                .is_err(),
            "the attachment is told the command was refused"
        );
    }

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

    #[test]
    fn bulk_selection_larger_than_command_capacity_drains_without_partial_admission() {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        feed.mark_snapshot_applied(snapshot("test", 1));
        let (sender, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(sender);
        set_grant(&feed.grant, true);
        feed.submit_batch((0..COMMAND_CAPACITY + 1).map(|index| {
            (
                format!("row-{index}"),
                SessionRequest::Mutation(SessionMutation::Stop),
            )
        }))
        .unwrap();
        assert_eq!(feed.pending.len(), COMMAND_CAPACITY);
        assert_eq!(feed.bulk_queue.len(), 1);
        assert!(feed
            .submit_batch([
                (
                    "new-row".into(),
                    SessionRequest::Mutation(SessionMutation::Stop)
                ),
                (
                    "row-0".into(),
                    SessionRequest::Mutation(SessionMutation::Stop)
                ),
            ])
            .is_err());
        assert_eq!(feed.bulk_queue.len(), 1);
        let mut submitted: Vec<_> = (0..COMMAND_CAPACITY)
            .map(|_| requests.try_recv().unwrap())
            .collect();
        assert!(requests.try_recv().is_err());
        assert!(submitted
            .pop()
            .unwrap()
            .result
            .send(Ok(CommandReply::Mutation(RuntimeCursor {
                epoch: "test".into(),
                revision: 2,
            })))
            .is_ok());
        feed.mark_snapshot_applied(snapshot("test", 2));
        assert!(feed.drain_command_errors().is_empty());
        assert_eq!(feed.pending.len(), COMMAND_CAPACITY);
        assert!(feed.bulk_queue.is_empty());
        let CommandRequest::Session { id, .. } = requests.try_recv().unwrap().request else {
            panic!("expected the queued row");
        };
        assert_eq!(id, format!("row-{COMMAND_CAPACITY}"));
    }

    #[tokio::test]
    async fn creation_tasks_are_reaped_while_the_writer_is_idle() {
        let app = axum::Router::new().route(
            "/api/sessions",
            axum::routing::post(|| async {
                (
                    [
                        (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                        (crate::daemon::RUNTIME_REVISION_HEADER, "2"),
                    ],
                    axum::Json(
                        serde_json::json!({"id":"created", "status":"Idle", "view":"structured"}),
                    ),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::daemon::DaemonClient::new(&format!("http://{addr}"), None).unwrap();
        let (requests, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        let (count, mut observed) = tokio::sync::watch::channel(0);
        let writer = tokio::spawn(run_command_writer(
            client,
            "test".into(),
            Arc::new(AtomicU64::new(1)),
            receiver,
            Some(count),
        ));
        for index in 0..12 {
            let (result, received) = tokio::sync::oneshot::channel();
            let body = serde_json::from_value(serde_json::json!({"path":"/tmp", "tool":"claude"}))
                .unwrap();
            requests
                .send(SessionCommand {
                    request: CommandRequest::Create({
                        let mut body: Box<crate::daemon::CreateSessionBody> = Box::new(body);
                        body.idempotency_key = Some(format!("stub-{index}"));
                        body
                    }),
                    lease: 1,
                    native: None,
                    result,
                })
                .await
                .unwrap();
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(3), received)
                    .await
                    .unwrap()
                    .unwrap(),
                Ok(CommandReply::Created(_))
            ));
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if *observed.borrow_and_update() == 0 {
                    break;
                }
                observed.changed().await.unwrap();
            }
        })
        .await
        .expect("completed creates must be removed without another request");
        assert!(
            !requests.is_closed(),
            "the writer remains idle and connected"
        );
        drop(requests);
        writer.await.unwrap();
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn writer_shutdown_and_abort_cancel_owned_open_creation_tasks() {
        for abort_writer in [false, true] {
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let entered = Arc::new(std::sync::Mutex::new(Some(entered)));
            let release = Arc::new(tokio::sync::Notify::new());
            let app = axum::Router::new().route(
                "/api/sessions",
                axum::routing::post({
                    let release = release.clone();
                    move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.lock().unwrap().take().unwrap().send(()).unwrap();
                            release.notified().await;
                            (axum::http::StatusCode::UNPROCESSABLE_ENTITY, "refused")
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = crate::daemon::DaemonClient::new(&format!("http://{addr}"), None).unwrap();
            let (requests, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
            let writer = tokio::spawn(run_command_writer(
                client,
                "test".into(),
                Arc::new(AtomicU64::new(1)),
                receiver,
                None,
            ));
            let (result, mut received) = tokio::sync::oneshot::channel();
            let body = serde_json::from_value(serde_json::json!({"path":"/tmp", "tool":"claude"}))
                .unwrap();
            requests
                .send(SessionCommand {
                    request: CommandRequest::Create({
                        let mut body: Box<crate::daemon::CreateSessionBody> = Box::new(body);
                        body.idempotency_key = Some("stub".into());
                        body
                    }),
                    lease: 1,
                    native: None,
                    result,
                })
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                received.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            if abort_writer {
                writer.abort();
            } else {
                drop(requests);
            }
            let stopped = tokio::time::timeout(std::time::Duration::from_secs(3), writer)
                .await
                .unwrap();
            if abort_writer {
                assert!(stopped.unwrap_err().is_cancelled());
            } else {
                stopped.unwrap();
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(3), received)
                    .await
                    .unwrap()
                    .is_err(),
                "the owned HTTP task drops its reply sender before the handler is released"
            );
            release.notify_waiters();
            server.abort();
            let _ = server.await;
        }
    }

    #[test]
    fn local_cancellation_does_not_hide_an_unknown_native_outcome() {
        for close_reply in [false, true] {
            let (mut feed, mut requests) = feed_with_native_identity("native");
            let mut preparation = feed.ensure_agent("native".into(), None).unwrap();
            preparation.lease.cancel_continuation();
            let command = requests.try_recv().unwrap();
            if close_reply {
                drop(command);
            } else {
                command
                    .result
                    .send(Err(CommandFailure::Unknown("transport interrupted".into())))
                    .unwrap();
            }
            let errors = feed.drain_command_errors();
            assert_eq!(errors.len(), 1);
            assert!(errors[0].outcome_unknown);
            assert!(preparation.result.try_recv().unwrap().is_err());
            assert!(!feed.can_submit("native"));
            let next = snapshot("test", 2);
            feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
            feed.mark_snapshot_applied(next);
            assert!(
                !feed.can_submit("native"),
                "a newer snapshot cannot resolve Unknown automatically"
            );
            feed.resolve_indeterminate("native").unwrap();
            assert!(feed.can_submit("native"));
        }
    }

    #[test]
    fn creation_receipts_wait_for_applied_same_epoch_and_survive_repeated_drains() {
        let (mut feed, mut requests) = feed_with_native_lane();
        let body =
            serde_json::from_value(serde_json::json!({"path":"/tmp", "tool":"claude"})).unwrap();
        feed.create_session("stub".into(), body).unwrap();
        let row = serde_json::from_value(
            serde_json::json!({"id":"created", "status":"Idle", "view":"structured"}),
        )
        .unwrap();
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Ok(CommandReply::Created(Box::new(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: row,
            }))))
            .unwrap();
        assert!(feed.drain_creation_results().is_empty());
        feed.publish_for_test(SessionFeedResult::Snapshot(snapshot("test", 2)));
        assert!(feed.drain_creation_results().is_empty());
        feed.mark_snapshot_applied(snapshot("foreign", 99));
        assert!(feed.drain_creation_results().is_empty());
        feed.mark_snapshot_applied(snapshot("test", 2));
        let results = feed.drain_creation_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "stub");
        assert_eq!(results[0].1.as_ref().unwrap().outcome.id, "created");
        assert!(feed.drain_creation_results().is_empty());
    }

    #[test]
    fn a_buffered_creation_receipt_does_not_restore_a_revoked_authority_generation() {
        let (mut feed, mut requests) = feed_with_native_lane();
        let body =
            serde_json::from_value(serde_json::json!({"path":"/tmp", "tool":"claude"})).unwrap();
        feed.create_session("stub".into(), body).unwrap();
        let row = serde_json::from_value(
            serde_json::json!({"id":"created", "status":"Idle", "view":"structured"}),
        )
        .unwrap();
        requests
            .try_recv()
            .unwrap()
            .result
            .send(Ok(CommandReply::Created(Box::new(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: row,
            }))))
            .unwrap();
        assert!(feed.drain_creation_results().is_empty());
        set_grant(&feed.grant, false);
        set_grant(&feed.grant, true);
        feed.mark_snapshot_applied(snapshot("test", 2));
        let outcomes = feed.drain_creation_results();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, Err(CommandFailure::Unknown(_))));
        assert!(feed.drain_creation_results().is_empty());
    }

    #[test]
    fn watch_replays_only_unapplied_snapshots_and_compares_both_cursor_fields() {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("old", 2)));
        let SessionFeedResult::Snapshot(first) = feed.try_recv().unwrap() else {
            unreachable!()
        };
        assert!(matches!(
            feed.try_recv(),
            Ok(SessionFeedResult::Snapshot(_))
        ));
        feed.mark_snapshot_applied(first);
        assert!(matches!(feed.try_recv(), Err(TryRecvError::Empty)));
        feed.publish_for_test(SessionFeedResult::Snapshot(snapshot("new", 2)));
        let SessionFeedResult::Snapshot(new) = feed.try_recv().unwrap() else {
            unreachable!()
        };
        assert_eq!(new.cursor.epoch, "new");
        assert!(matches!(
            feed.try_recv(),
            Ok(SessionFeedResult::Snapshot(_))
        ));
        feed.publish_for_test(SessionFeedResult::Unavailable("lost".into()));
        assert!(matches!(
            feed.try_recv(),
            Ok(SessionFeedResult::Unavailable(_))
        ));
        assert!(matches!(feed.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn quit_cancellation_bypasses_a_blocked_writer_and_waits_for_ack() {
        use std::sync::Mutex;
        let (entered, blocked) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let stop_release = Arc::new(tokio::sync::Notify::new());
        let (cancel_seen, cancel_received) = tokio::sync::oneshot::channel();
        let cancel_seen = Arc::new(Mutex::new(Some(cancel_seen)));
        let ack = Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new()
            .route(
                "/api/sessions/other/stop",
                axum::routing::post({
                    let entered = entered.clone();
                    let release = stop_release.clone();
                    move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.lock().unwrap().take().unwrap().send(()).unwrap();
                            release.notified().await;
                            axum::http::StatusCode::CONFLICT
                        }
                    }
                }),
            )
            .route(
                "/api/sessions/creation/creation/cancel",
                axum::routing::post({
                    let seen = cancel_seen.clone();
                    let ack = ack.clone();
                    move || {
                        let seen = seen.clone();
                        let ack = ack.clone();
                        async move {
                            seen.lock().unwrap().take().unwrap().send(()).unwrap();
                            ack.notified().await;
                            (
                                [
                                    (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                                    (crate::daemon::RUNTIME_REVISION_HEADER, "2"),
                                ],
                                "",
                            )
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = crate::daemon::DaemonClient::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            None,
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        feed.mark_snapshot_applied(snapshot("test", 1));
        set_grant(&feed.grant, true);
        set_grant(&feed.grant, false);
        set_grant(&feed.grant, true);
        feed.quit_control.send_replace(Some(QuitCancellationHandle {
            client: client.clone(),
            epoch: "test".into(),
            grant: feed.grant.clone(),
        }));
        let (sender, requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(sender);
        let writer = tokio::spawn(run_command_writer(
            client,
            "test".into(),
            feed.grant.clone(),
            requests,
            None,
        ));
        feed.enqueue(
            "other".into(),
            SessionRequest::Mutation(SessionMutation::Stop),
            None,
            None,
        )
        .unwrap();
        blocked.await.unwrap();
        let mut cancellation =
            Box::pin(feed.cancel_creation_at_quit("key", Some("creation"), "test"));
        assert!(futures_util::poll!(&mut cancellation).is_pending());
        tokio::select! {
            result = &mut cancellation => panic!("quit cancellation answered before ACK: {result:?}"),
            result = cancel_received => result.unwrap(),
        }
        assert!(
            futures_util::poll!(&mut cancellation).is_pending(),
            "request arrival is not ACK"
        );
        ack.notify_one();
        assert_eq!(cancellation.await.unwrap().revision, 2);
        assert!(matches!(
            feed.cancel_creation_at_quit("key", Some("creation"), "old")
                .await,
            Err(CommandFailure::Rejected(_))
        ));
        let grant = feed.grant.clone();
        let progress = feed.progress.clone();
        let mut cancellation = Box::pin(feed.cancel_creation_at_quit("key", None, "test"));
        assert!(futures_util::poll!(&mut cancellation).is_pending());
        set_grant(&grant, false);
        set_grant(&grant, true);
        progress.send_replace(Some(vec![]));
        assert!(matches!(
            cancellation.await,
            Err(CommandFailure::Rejected(_))
        ));
        stop_release.notify_one();
        writer.abort();
        server.abort();
    }

    #[tokio::test]
    async fn quit_cancellation_resolves_only_its_exact_progress_key() {
        let (seen, received) = tokio::sync::oneshot::channel();
        let seen = Arc::new(std::sync::Mutex::new(Some(seen)));
        let app = axum::Router::new().route(
            "/api/sessions/{id}/creation/cancel",
            axum::routing::post({
                let seen = seen.clone();
                move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().take().unwrap().send(id).unwrap();
                        (
                            [
                                (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                                (crate::daemon::RUNTIME_REVISION_HEADER, "3"),
                            ],
                            "",
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = crate::daemon::DaemonClient::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            None,
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        set_grant(&feed.grant, true);
        feed.quit_control.send_replace(Some(QuitCancellationHandle {
            client,
            epoch: "test".into(),
            grant: feed.grant.clone(),
        }));
        let progress = feed.progress.clone();
        let mut cancelling = Box::pin(feed.cancel_creation_at_quit("exact-key", None, "test"));
        assert!(futures_util::poll!(&mut cancelling).is_pending());
        let entry = |key: &str, id: &str| {
            serde_json::from_value(serde_json::json!({
                "request_key": key, "session_id": id, "title": "same title", "profile": "test",
                "phase": "create_hooks", "command": null, "output": [], "cancelled": false,
            }))
            .unwrap()
        };
        progress.send_replace(Some(vec![entry("unrelated", "wrong")]));
        assert!(futures_util::poll!(&mut cancelling).is_pending());
        progress.send_replace(Some(vec![
            entry("unrelated", "wrong"),
            entry("exact-key", "original"),
        ]));
        assert_eq!(cancelling.await.unwrap().revision, 3);
        assert_eq!(received.await.unwrap(), "original");
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn quit_cancellation_bounds_unresolved_creation_identity() {
        let mut feed =
            SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(snapshot("test", 1)));
        set_grant(&feed.grant, true);
        feed.quit_control.send_replace(Some(QuitCancellationHandle {
            client: crate::daemon::DaemonClient::new("http://127.0.0.1:1", None).unwrap(),
            epoch: "test".into(),
            grant: feed.grant.clone(),
        }));
        let mut cancelling = Box::pin(feed.cancel_creation_at_quit("missing-key", None, "test"));
        assert!(futures_util::poll!(&mut cancelling).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert!(matches!(cancelling.await, Err(CommandFailure::Unknown(_))));
    }

    #[tokio::test]
    async fn cancellation_is_sent_while_create_http_request_is_still_open() {
        use std::sync::Mutex;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered_tx)));
        let release = Arc::new(tokio::sync::Notify::new());
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(Mutex::new(Some(cancelled_tx)));
        let app = axum::Router::new()
            .route(
                "/api/sessions",
                axum::routing::post({
                    let entered = entered.clone();
                    let release = release.clone();
                    move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            if let Some(tx) = entered.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            release.notified().await;
                            (
                                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                                "creation refused",
                            )
                        }
                    }
                }),
            )
            .route(
                "/api/sessions/{id}/creation/cancel",
                axum::routing::post({
                    let cancelled = cancelled.clone();
                    move || {
                        let cancelled = cancelled.clone();
                        async move {
                            if let Some(tx) = cancelled.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            (
                                [
                                    (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                                    (crate::daemon::RUNTIME_REVISION_HEADER, "2"),
                                ],
                                "",
                            )
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::daemon::DaemonClient::new(&format!("http://{addr}"), None).unwrap();
        let (requests, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        let writer = tokio::spawn(run_command_writer(
            client,
            "test".into(),
            Arc::new(AtomicU64::new(1)),
            receiver,
            None,
        ));
        let (create_tx, mut create_rx) = tokio::sync::oneshot::channel();
        let body =
            serde_json::from_value(serde_json::json!({"path":"/tmp","tool":"claude"})).unwrap();
        requests
            .send(SessionCommand {
                request: CommandRequest::Create({
                    let mut body: Box<crate::daemon::CreateSessionBody> = Box::new(body);
                    body.idempotency_key = Some("stub".into());
                    body
                }),
                lease: 1,
                native: None,
                result: create_tx,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        requests
            .send(SessionCommand {
                request: CommandRequest::Session {
                    id: "daemon-row".into(),
                    request: SessionRequest::CancelCreation,
                },
                lease: 1,
                native: None,
                result: cancel_tx,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), cancelled_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), cancel_rx)
                .await
                .unwrap()
                .unwrap(),
            Ok(CommandReply::Mutation(_))
        ));
        assert!(matches!(
            create_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), create_rx)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        drop(requests);
        writer.await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn native_attach_ensures_agent_without_start_or_restart() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new().route(
            "/api/sessions/id/ensure",
            axum::routing::post({
                let seen = seen.clone();
                move || {
                    seen.lock().unwrap().push("ensure");
                    async {
                        (
                            [
                                (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                                (crate::daemon::RUNTIME_REVISION_HEADER, "3"),
                            ],
                            axum::Json(
                                serde_json::json!({"tmux_session": "pane", "status": "alive"}),
                            ),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::daemon::DaemonClient::new(&format!("http://{addr}"), None).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        let writer = tokio::spawn(run_command_writer(
            client,
            "test".into(),
            Arc::new(AtomicU64::new(1)),
            rx,
            None,
        ));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(SessionCommand {
            request: CommandRequest::Session {
                id: "id".into(),
                request: SessionRequest::EnsureAgent { size: None },
            },
            lease: 1,
            native: Some(NativeLease::valid_for_test()),
            result: reply_tx,
        })
        .await
        .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(3), reply_rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(reply, CommandReply::Terminal(receipt)
            if receipt.outcome.tmux_session == "pane" && receipt.cursor.revision == 3));
        assert_eq!(*seen.lock().unwrap(), ["ensure"]);
        drop(tx);
        writer.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn agent_ensure_failures_preserve_refusal_quarantine_and_commit_fences() {
        use axum::response::IntoResponse;
        for (ensure_status, committed_revision) in [
            (axum::http::StatusCode::FORBIDDEN, None),
            (axum::http::StatusCode::CONFLICT, None),
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, None),
            (axum::http::StatusCode::OK, None),
            (axum::http::StatusCode::CONFLICT, Some("1")),
            (axum::http::StatusCode::CONFLICT, Some("2")),
            (axum::http::StatusCode::CONFLICT, Some("3")),
        ] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let app = axum::Router::new().route(
                "/api/sessions/id/ensure",
                axum::routing::post({
                    let seen = seen.clone();
                    move || {
                        seen.lock().unwrap().push("ensure");
                        async move {
                            if let Some(revision) = committed_revision {
                                return (
                                    ensure_status,
                                    [
                                        (
                                            crate::daemon::ERROR_CODE_HEADER,
                                            "managed_resume_refused",
                                        ),
                                        (crate::daemon::RUNTIME_EPOCH_HEADER, "test"),
                                        (crate::daemon::RUNTIME_REVISION_HEADER, revision),
                                    ],
                                    "private pane refusal detail",
                                )
                                    .into_response();
                            }
                            (
                                ensure_status,
                                [(crate::daemon::ERROR_CODE_HEADER, "resume_failed")],
                                "pane preparation refused",
                            )
                                .into_response()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = crate::daemon::DaemonClient::new(
                &format!("http://{}", listener.local_addr().unwrap()),
                None,
            )
            .unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (mut feed, mut requests) = feed_with_native_identity("id");
            let mut preparation = feed.ensure_agent("id".into(), None).unwrap();
            let mut command = requests.recv().await.unwrap();
            let (completed, completion) = tokio::sync::oneshot::channel();
            let original = std::mem::replace(&mut command.result, completed);
            let (commands, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
            let writer = tokio::spawn(run_command_writer(
                client,
                "test".into(),
                feed.grant.clone(),
                receiver,
                None,
            ));
            commands.send(command).await.unwrap();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(3), completion)
                .await
                .unwrap()
                .unwrap();
            if let Some(revision) = committed_revision {
                assert!(matches!(&reply,
                    Ok(CommandReply::CommittedFailure { cursor, failure: CommandFailure::Rejected(_) })
                    if cursor.revision == revision.parse::<u64>().unwrap()));
            } else {
                assert!(
                    matches!(
                        &reply,
                        Err(CommandFailure::Unknown(_))
                            if ensure_status.is_server_error() || ensure_status.is_success()
                    ) || matches!(
                        &reply,
                        Err(CommandFailure::Rejected(_)) if ensure_status.is_client_error()
                    )
                );
            }
            original.send(reply).unwrap();
            let mut errors = feed.drain_command_errors();
            if let Some(revision) = committed_revision {
                let revision = revision.parse::<u64>().unwrap();
                if revision > 1 {
                    assert!(errors.is_empty());
                    assert!(matches!(
                        preparation.result.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ));
                    assert!(!feed.can_submit("id"));
                    feed.mark_snapshot_applied(snapshot("test", revision - 1));
                    assert!(feed.drain_command_errors().is_empty());
                    assert!(!feed.can_submit("id"));
                    feed.mark_snapshot_applied(snapshot("test", revision));
                    errors = feed.drain_command_errors();
                }
            }
            assert!(preparation.result.try_recv().unwrap().is_err());
            let unknown = ensure_status.is_server_error() || ensure_status.is_success();
            assert_eq!(feed.can_submit("id"), !unknown);
            assert_eq!(errors.iter().any(|error| error.outcome_unknown), unknown);
            assert_eq!(*seen.lock().unwrap(), ["ensure"]);
            drop(commands);
            writer.await.unwrap();
            server.abort();
            let _ = server.await;
        }
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

    #[tokio::test]
    async fn acknowledged_change_waits_for_applied_snapshot_and_current_permission() {
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
                if revoke {
                    assert!(!feed.can_submit("session"));
                } else {
                    assert!(feed.can_submit("session"));
                    feed.submit("session".into(), SessionMutation::Stop)
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn ensure_agent_rejects_receipt_superseded_by_stop_or_restart() {
        let row: crate::daemon::SessionResponse = serde_json::from_value(serde_json::json!({
            "id": "session",
            "profile": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "lifecycle_generation": 4,
            "status": "Running",
            "agent_pane": {"state": "alive", "tmux_session": "agent-4", "physical": native_physical_fixture()}
        }))
        .unwrap();
        let mut initial = snapshot("test", 1);
        Arc::make_mut(&mut initial).contents.sessions.push(row);
        let mut feed = SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(initial.clone()));
        feed.mark_snapshot_applied(initial.clone());
        let (commands, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(commands);
        set_grant(&feed.native_grant, true);
        let mut prepared = feed.ensure_agent("session".into(), None).unwrap();
        let command = requests.try_recv().unwrap();
        assert!(matches!(
            command.request,
            CommandRequest::Session {
                request: SessionRequest::EnsureAgent { .. },
                ..
            }
        ));
        command
            .result
            .send(Ok(CommandReply::Terminal(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),

                    revision: 2,
                },
                outcome: TerminalTarget {
                    physical: Some(native_physical_fixture()),
                    tmux_session: "agent-3".into(),
                    status: crate::daemon::TerminalTargetStatus::Alive,
                    lifecycle_generation: 3,
                    profile: "test".into(),
                },
            })))
            .unwrap();
        let mut next = initial;
        Arc::make_mut(&mut next).cursor.revision = 2;
        feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
        assert!(feed.drain_command_errors().is_empty());
        feed.mark_snapshot_applied(next);
        assert!(feed.drain_command_errors().is_empty());
        assert!(matches!(prepared.result.try_recv(), Ok(Err(_))));
    }

    #[test]
    fn ensure_agent_accepts_starting_snapshot_after_pane_observation() {
        let row: crate::daemon::SessionResponse = serde_json::from_value(serde_json::json!({
            "id": "session",
            "profile": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "lifecycle_generation": 4,
            "status": "Starting"
        }))
        .unwrap();
        let mut initial = snapshot("test", 1);
        Arc::make_mut(&mut initial).contents.sessions.push(row);
        let mut feed = SessionFeed::seeded_for_test(SessionFeedResult::Snapshot(initial.clone()));
        feed.mark_snapshot_applied(initial.clone());
        let (commands, mut requests) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
        feed.commands = Some(commands);
        set_grant(&feed.native_grant, true);
        let mut prepared = feed.ensure_agent("session".into(), None).unwrap();
        let command = requests.try_recv().unwrap();
        assert!(matches!(
            command.request,
            CommandRequest::Session {
                request: SessionRequest::EnsureAgent { .. },
                ..
            }
        ));
        command
            .result
            .send(Ok(CommandReply::Terminal(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: TerminalTarget {
                    physical: Some(native_physical_fixture()),
                    tmux_session: "agent-4".into(),
                    status: crate::daemon::TerminalTargetStatus::Alive,
                    lifecycle_generation: 4,
                    profile: "test".into(),
                },
            })))
            .unwrap();
        let mut next = initial;
        Arc::make_mut(&mut next).cursor.revision = 2;
        Arc::make_mut(&mut next).contents.sessions[0].agent_pane =
            crate::session::PaneObservation {
                state: crate::session::PanePresence::Alive,
                tmux_session: Some("agent-4".into()),
                physical: Some(native_physical_fixture()),
                legacy_tool: None,
            };
        feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
        assert!(feed.drain_command_errors().is_empty());
        feed.mark_snapshot_applied(next);
        assert!(feed.drain_command_errors().is_empty());
        assert!(prepared.result.try_recv().unwrap().is_ok());
    }
    #[test]
    fn queued_live_preparation_accepts_renamed_label_without_rebinding_physical_owner() {
        let (mut feed, mut requests) = feed_with_native_identity("session");
        let mut prepared = feed.ensure_agent("session".into(), None).unwrap();
        let command = requests.try_recv().unwrap();
        let original = prepared.lease.target().unwrap().clone();
        let mut renamed = native_identity_snapshot(2);
        Arc::make_mut(&mut renamed).contents.sessions[0]
            .agent_pane
            .tmux_session = Some("renamed-native-label".into());
        feed.publish_for_test(SessionFeedResult::Snapshot(renamed.clone()));
        feed.mark_snapshot_applied(renamed);
        assert!(
            prepared.lease.can_dispatch(),
            "name-only metadata refused queued work"
        );
        command
            .result
            .send(Ok(CommandReply::Terminal(MutationReceipt {
                cursor: RuntimeCursor {
                    epoch: "test".into(),
                    revision: 2,
                },
                outcome: TerminalTarget {
                    tmux_session: "renamed-native-label".into(),
                    physical: original.physical.clone(),
                    lifecycle_generation: original.lifecycle_generation,
                    profile: original.profile.clone(),
                    status: crate::daemon::TerminalTargetStatus::Alive,
                },
            })))
            .unwrap();
        assert!(feed.drain_command_errors().is_empty());
        let accepted = prepared.result.try_recv().unwrap().unwrap();
        assert!(Arc::ptr_eq(&accepted, &original));
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
            Profile,
            Generation,
            Physical,
            Presence(PanePresence),
            TargetName(Option<&'static str>),
        }
        for invalidation in [
            Invalidation::None,
            Invalidation::Cancel,
            Invalidation::Permission,
            Invalidation::Deleted,
            Invalidation::Profile,
            Invalidation::Generation,
            Invalidation::Physical,
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
                "id": "session", "profile": "test", "lifecycle_generation": 4,
                "created_at": "2026-01-01T00:00:00Z",
                "auxiliary": [{
                    "target": target, "state": "alive",
                    "tmux_session": "daemon-owned-target", "physical": native_physical_fixture()
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
                .ensure_auxiliary("session".into(), target.clone(), None, None)
                .is_err());
            assert!(feed.can_submit("session"));
            set_grant(&feed.native_grant, true);
            let mut prepared = feed
                .ensure_auxiliary("session".into(), target, None, None)
                .unwrap();
            let mut next = initial;
            Arc::make_mut(&mut next).cursor.revision = 2;
            match invalidation {
                Invalidation::None => {}
                Invalidation::Cancel => prepared.lease.cancel_continuation(),
                Invalidation::Permission => {
                    set_grant(&feed.native_grant, false);
                    set_grant(&feed.native_grant, true);
                }
                Invalidation::Deleted => Arc::make_mut(&mut next).contents.sessions.clear(),
                Invalidation::Profile => {
                    Arc::make_mut(&mut next).contents.sessions[0].profile = "other".into();
                }
                Invalidation::Generation => {
                    Arc::make_mut(&mut next).contents.sessions[0].lifecycle_generation += 1;
                }
                Invalidation::Physical => {
                    let row = &mut Arc::make_mut(&mut next).contents.sessions[0];
                    let mut value = serde_json::to_value(&*row).unwrap();
                    value["auxiliary"][0]["physical"]["identity"]["pane_id"] =
                        serde_json::json!("%99");
                    *row = serde_json::from_value(value).unwrap();
                }
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
                    physical: Some(native_physical_fixture()),
                    tmux_session: "daemon-owned-target".into(),
                    status: crate::daemon::TerminalTargetStatus::Exists,
                    lifecycle_generation: 4,
                    profile: "test".into(),
                },
            }));
            feed.publish_for_test(SessionFeedResult::Snapshot(next.clone()));
            let errors = feed.drain_command_errors();
            if matches!(invalidation, Invalidation::Permission) {
                assert_eq!(errors.len(), 1);
                assert!(errors[0].outcome_unknown);
                assert!(prepared.result.try_recv().unwrap().is_err());
                feed.mark_snapshot_applied(next);
                assert!(feed.drain_command_errors().is_empty());
                assert!(!feed.can_submit("session"));
                continue;
            }
            assert!(errors.is_empty());
            assert!(matches!(
                prepared.result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            feed.mark_snapshot_applied(next);
            let errors = feed.drain_command_errors();
            assert!(errors.is_empty());
            let ready = prepared.result.try_recv().unwrap();
            assert_eq!(
                ready.is_ok(),
                matches!(
                    invalidation,
                    Invalidation::None | Invalidation::TargetName(_)
                ),
                "{invalidation:?}"
            );
            assert!(feed.can_submit("session"));
            feed.submit("session".into(), SessionMutation::Stop)
                .unwrap();
        }
    }
}

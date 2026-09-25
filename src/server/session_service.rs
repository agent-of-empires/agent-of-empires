//! Shared session-domain service handle.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::server::session_spawn::{spawn_structured_session, SpawnOutcome, StructuredSessionSpec};
use crate::session::{Instance, PluginCreateIdempotency};

/// A create currently being built for a `(plugin_id, idempotency_key)` scope.
struct CreateInFlight {
    payload_hash: String,
    notify: Arc<tokio::sync::Notify>,
}

/// What `try_claim_in_flight` decided for a plugin create request.
enum ClaimOutcome {
    /// This caller owns the build; it must drop the returned guard on every
    /// exit path so waiters wake up.
    Claimed,
    /// An identical request is mid-build; wait on the notify, then re-check.
    Wait(Arc<tokio::sync::Notify>),
    /// The same key is mid-build with a different payload.
    Conflict,
}

/// Marker error for a plugin create that reused an idempotency key with a different request
/// payload.
#[derive(Debug)]
pub(crate) struct IdempotencyConflict {
    pub key: String,
}

impl std::fmt::Display for IdempotencyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "idempotency key {:?} was already used with a different request payload",
            self.key
        )
    }
}

impl std::error::Error for IdempotencyConflict {}

/// Read-only resolution of a plugin create-idempotency key, so a caller can
/// decide whether to charge admission before building the session (#2897).
pub(crate) enum CreateIdempotencyProbe {
    /// A prior create with this plugin/key/payload already exists; replay it.
    Replay(Box<Instance>),
    /// No prior create matches; this is a genuinely new create.
    New,
}

/// Result of matching a plugin create request against the persisted sessions.
enum IdempotentMatch {
    /// Same plugin, key, and payload: return this existing session.
    Same(Box<Instance>),
    /// Same plugin and key, different payload: refuse.
    Conflict,
    /// No session carries this plugin/key pair.
    None,
}

/// The `Instance` fields `mutate_instance_persisted` copies from memory to disk.
struct MirroredFields {
    queued_prompts: Vec<crate::daemon::QueuedPromptEntry>,
    queued_prompt_next_seq: u64,
    idle_dormant_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Mirrored as `disk = max(disk, memory)`, not copied, because the field is documented
    /// monotone non-decreasing and disk can legitimately lead memory when a peer process
    /// touched the row.
    last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of `SessionService::edit_queued_prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditQueuedOutcome {
    Updated,
    /// No row with that `prompt_id` in the session's queue.
    NotFound,
    /// The edit would leave a row with neither text nor attachments, which the drain cannot
    /// deliver.
    WouldEmpty,
}

/// Pick the leading drain batch from a session's queue and its combined text, matching the
/// client's `useAcpSession` split exactly.
fn queue_drain_batch<'a>(
    queue: &'a [crate::daemon::QueuedPromptEntry],
    profile: &crate::acp::agent_profiles::AgentProfile,
) -> (&'a [crate::daemon::QueuedPromptEntry], String) {
    if queue.is_empty() {
        return (&[], String::new());
    }
    let batch_end = if profile.clear_aliases.is_empty() {
        queue.len()
    } else if profile.is_clear_command(&queue[0].text) {
        1
    } else {
        queue
            .iter()
            .position(|e| profile.is_clear_command(&e.text))
            .unwrap_or(queue.len())
    };
    let sub = &queue[..batch_end];
    let combined = sub
        .iter()
        .map(|e| e.text.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (sub, combined)
}

pub struct SessionService {
    /// Live in-memory session list, shared with `AppState.instances`.
    pub instances: Arc<RwLock<Vec<Instance>>>,
    /// Per-instance mutation locks, shared with `AppState.instance_locks`.
    pub instance_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Storage change-notification service, shared with `AppState.file_watch`.
    pub file_watch: Arc<crate::file_watch::FileWatchService>,
    /// Opt-in telemetry create counter, shared with `AppState.telemetry_session_creates`.
    pub telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
    /// Shared with `AppState.mutation_epoch`. Bumped under the `instances` write lock by
    /// any change a disk snapshot read earlier would not carry, so that reload drops
    /// itself instead of overwriting the change.
    pub mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Owns the per-session ACP agent subprocesses, shared with `AppState.acp_supervisor`.
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    /// Durable ACP event store, shared with `AppState.acp_event_store`. Used by the
    /// pending-turn drain to reload attachment blobs for a rate-limit resume continuation.
    pub acp_event_store: Arc<crate::acp::event_store::EventStore>,
    /// Live control-state projection, shared with `AppState.acp_control_cache`.
    /// The queue drain reads turn liveness from it so it agrees with prompt
    /// dispatch; see [`SessionService::fold_control_state`].
    pub acp_control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
    /// In-flight plugin creates keyed by `(plugin_id, idempotency_key)`.
    // ponytail.
    create_in_flight: std::sync::Mutex<HashMap<(String, String), CreateInFlight>>,
    /// Session ids with a pending-initial-turn drain in flight, so the create fast path and
    /// the reconciler tick cannot queue duplicate drains.
    pending_drains: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Per-session persist locks for `mutate_instance_persisted`, held across snapshot AND
    /// disk write so the two cannot be reordered.
    persist_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Per-session prompt-submission locks.
    prompt_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Test-only tap on [`SessionService::prompt_submission`], fired before it
    /// reaches `prompt_locks`. See [`SessionService::watch_submission_claims`].
    #[cfg(test)]
    submission_claims: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<String>>,
    #[cfg(test)]
    pub(super) created_instance_gate: std::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

/// Who is asking the session service to act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCaller {
    /// A human-facing surface (HTTP dashboard, TUI).
    User,
    /// A plugin worker, identified by its connection's plugin id.
    Plugin { plugin_id: String },
}

/// Why a caller may not open a turn on a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnAdmissionError {
    /// No session with this id.
    SessionNotFound,
    /// A plugin caller targeted a session it did not create.
    NotOwner,
}

impl From<TurnAdmissionError> for SendTurnError {
    fn from(e: TurnAdmissionError) -> Self {
        match e {
            TurnAdmissionError::SessionNotFound => Self::SessionNotFound,
            TurnAdmissionError::NotOwner => Self::NotOwner,
        }
    }
}

/// Typed outcome of [`SessionService::send_turn`], split by whether the failure happened
/// before or after the prompt was published into the event stream, so callers can map each
/// stage faithfully (the HTTP handler keeps its exact pre-extraction status codes, and only
/// fires the post-publish smart-rename hook when a publish actually happened).
pub(crate) enum SendTurnError {
    /// Pre-publish.
    SessionNotFound,
    /// Pre-publish.
    NotOwner,
    /// Pre-publish.
    ModeApplication(crate::acp::supervisor::SupervisorError),
    /// Pre-publish.
    ResumeFailed(crate::acp::supervisor::SupervisorError),
    /// Pre-publish.
    WorkerNotReady,
    /// Pre-publish: `no_revive` was set and delivering this turn would have
    /// required resuming a worker that is not currently running.
    RevivalRefused,
    /// Post-publish: the forward to the agent failed.
    Send(crate::acp::supervisor::SupervisorError),
}

/// Outcome of [`SessionService::touch_and_wake_on_prompt`].
pub(crate) enum PromptTouch {
    /// Touched (and woken, if archived/snoozed/idle-dormant); carries
    /// whether this specifically was the idle-dormant wake, the flag
    /// `prompt_dispatch_under_submission` and `send_turn` need.
    Touched { idle_dormant: bool },
    /// `no_revive` was set and the session needed archived/snoozed/
    /// idle-dormant revival to accept this prompt; refused before any
    /// mutation.
    RevivalRefused,
}

impl PromptTouch {
    /// The idle-dormant flag for a `Touched` outcome. Only meaningful when
    /// the caller passed `no_revive: false`, which never produces
    /// `RevivalRefused`.
    pub(crate) fn idle_dormant(&self) -> bool {
        matches!(self, PromptTouch::Touched { idle_dormant: true })
    }
}

/// Everything [`SessionService::send_turn`] needs beyond the caller and
/// session id, bundled so the function stays under clippy's argument-count
/// lint.
pub(crate) struct SendTurnRequest<'a> {
    pub text: &'a str,
    pub attachments: &'a [crate::acp::event_store::AttachmentBlob],
    /// Forces the resume trigger even when the worker looks alive, mirroring
    /// the handler's idle-dormant wake (#1689).
    pub woke_idle_dormant: bool,
    pub prompt_id: Option<String>,
    /// True when the daemon queued this turn itself (a rate-limit resume
    /// continuation) rather than the user typing it just now, so the
    /// transcript model can skip rendering a duplicate row for it.
    pub synthesized: bool,
    /// Refuse rather than resume a worker that is not currently running
    /// (#4081 review: the `WorkerDown`-only check in `acp_prompt` misses a
    /// rate-limit-exhausted park, where dispatch is `Sent` but no worker is
    /// alive). Enforced here, at the same `is_running` check that decides
    /// whether to resume, rather than by a separate liveness probe that
    /// would race it.
    pub no_revive: bool,
}

impl std::fmt::Display for SendTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => write!(f, "session not found"),
            Self::NotOwner => write!(f, "session was not created by the calling plugin"),
            Self::ModeApplication(e) => write!(f, "mode application failed: {e}"),
            Self::ResumeFailed(e) => write!(f, "worker resume failed: {e}"),
            Self::WorkerNotReady => write!(f, "worker not ready"),
            Self::RevivalRefused => write!(f, "no_revive: reviving a stopped worker is required"),
            Self::Send(e) => write!(f, "prompt forward failed: {e}"),
        }
    }
}

/// The ACP collaborators `SessionService` shares with `AppState`.
pub struct AcpDeps {
    pub supervisor: Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    pub event_store: Arc<crate::acp::event_store::EventStore>,
    pub control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
}

impl SessionService {
    pub fn new(
        instances: Arc<RwLock<Vec<Instance>>>,
        instance_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
        file_watch: Arc<crate::file_watch::FileWatchService>,
        telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
        mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
        acp: AcpDeps,
    ) -> Self {
        Self {
            instances,
            instance_locks,
            file_watch,
            telemetry_session_creates,
            mutation_epoch,
            acp_supervisor: acp.supervisor,
            acp_event_store: acp.event_store,
            acp_control_cache: acp.control_cache,
            create_in_flight: std::sync::Mutex::new(HashMap::new()),
            pending_drains: std::sync::Mutex::new(std::collections::HashSet::new()),
            persist_locks: RwLock::new(HashMap::new()),
            prompt_locks: RwLock::new(HashMap::new()),
            #[cfg(test)]
            submission_claims: std::sync::OnceLock::new(),
            #[cfg(test)]
            created_instance_gate: std::sync::Mutex::new(None),
        }
    }

    /// Create a structured session through the shared spawn pipeline, optionally as a
    /// plugin with a create-idempotency key.
    pub(crate) async fn create_structured_session(
        self: &Arc<Self>,
        mut spec: StructuredSessionSpec,
        plugin_id: Option<&str>,
        idempotency_key: Option<&str>,
        initial_turn: Option<&str>,
    ) -> anyhow::Result<(SpawnOutcome, bool)> {
        // Persisted with the instance in the same Storage::update, so the
        // create and its first turn are accepted atomically; the drain paths
        // deliver it once the worker is live.
        spec.pending_initial_turn = initial_turn.map(str::to_string);
        let Some(plugin_id) = plugin_id else {
            let outcome = spawn_structured_session(self, spec).await?;
            return Ok((outcome, true));
        };

        spec.created_by_plugin = Some(plugin_id.to_string());
        // Fail-closed: install-time plugin consent is not repository trust.
        spec.trust_hooks = Some(false);

        let Some(key) = idempotency_key else {
            let outcome = spawn_structured_session(self, spec).await?;
            return Ok((outcome, true));
        };

        let payload_hash = spec_payload_hash(&spec);
        spec.plugin_create_idempotency = Some(PluginCreateIdempotency {
            key: key.to_string(),
            payload_hash: payload_hash.clone(),
        });
        let scope = (plugin_id.to_string(), key.to_string());

        loop {
            // Persisted-first lookup.
            {
                let instances = self.instances.read().await;
                match find_idempotent_match(&instances, plugin_id, key, &payload_hash) {
                    IdempotentMatch::Same(instance) => {
                        return Ok((
                            SpawnOutcome {
                                instance: *instance,
                                warnings: Vec::new(),
                            },
                            false,
                        ));
                    }
                    IdempotentMatch::Conflict => {
                        return Err(anyhow::Error::new(IdempotencyConflict {
                            key: key.to_string(),
                        }));
                    }
                    IdempotentMatch::None => {}
                }
            }
            match self.try_claim_in_flight(&scope, &payload_hash) {
                ClaimOutcome::Claimed => break,
                ClaimOutcome::Wait(notify) => {
                    // The winner removes its entry and notifies on every exit path (guard
                    // drop), after which the loop re-checks the persisted list.
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(250),
                        notify.notified(),
                    )
                    .await;
                }
                ClaimOutcome::Conflict => {
                    return Err(anyhow::Error::new(IdempotencyConflict {
                        key: key.to_string(),
                    }));
                }
            }
        }

        let _guard = InFlightGuard {
            service: Arc::clone(self),
            scope,
        };
        let outcome = spawn_structured_session(self, spec).await?;
        Ok((outcome, true))
    }

    /// Resolve a persisted plugin create-idempotency decision without any side effect, so a
    /// caller can charge admission (rate/concurrency) only for genuinely new creates.
    pub(crate) async fn probe_plugin_create_idempotency(
        &self,
        spec: &StructuredSessionSpec,
        plugin_id: &str,
        key: &str,
    ) -> Result<CreateIdempotencyProbe, IdempotencyConflict> {
        let payload_hash = spec_payload_hash(spec);
        let instances = self.instances.read().await;
        match find_idempotent_match(&instances, plugin_id, key, &payload_hash) {
            IdempotentMatch::Same(instance) => Ok(CreateIdempotencyProbe::Replay(instance)),
            IdempotentMatch::Conflict => Err(IdempotencyConflict {
                key: key.to_string(),
            }),
            IdempotentMatch::None => Ok(CreateIdempotencyProbe::New),
        }
    }

    /// Claim the in-flight slot for a `(plugin_id, key)` scope, or report an
    /// identical build to wait on / a payload conflict to refuse.
    fn try_claim_in_flight(&self, scope: &(String, String), payload_hash: &str) -> ClaimOutcome {
        let mut in_flight = self
            .create_in_flight
            .lock()
            .expect("create_in_flight mutex poisoned");
        match in_flight.get(scope) {
            Some(entry) if entry.payload_hash == payload_hash => {
                ClaimOutcome::Wait(entry.notify.clone())
            }
            Some(_) => ClaimOutcome::Conflict,
            None => {
                in_flight.insert(
                    scope.clone(),
                    CreateInFlight {
                        payload_hash: payload_hash.to_string(),
                        notify: Arc::new(tokio::sync::Notify::new()),
                    },
                );
                ClaimOutcome::Claimed
            }
        }
    }

    /// Record that a prompt is arriving. `no_revive` refuses atomically,
    /// under the same per-session lock, rather than waking an
    /// archived/snoozed/idle-dormant session (#4081 review: a client-side
    /// liveness check before this call would race a concurrent archive,
    /// snooze, or wake).
    pub(crate) async fn touch_and_wake_on_prompt(&self, id: &str, no_revive: bool) -> PromptTouch {
        let inst_lock = self.instance_lock(id).await;
        let _guard = inst_lock.lock().await;
        let (profile, wake, woke_idle_dormant) = {
            let mut instances = self.instances.write().await;
            let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
                return PromptTouch::Touched {
                    idle_dormant: false,
                };
            };
            let was_idle_dormant = inst.is_idle_dormant();
            let wake = inst.is_archived() || inst.is_snoozed() || was_idle_dormant;
            if no_revive && wake {
                return PromptTouch::RevivalRefused;
            }
            inst.touch_last_accessed();
            self.invalidate_disk_snapshots();
            if was_idle_dormant {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    "waking idle-dormant structured view session on prompt; spawning a fresh worker"
                );
            }
            (inst.source_profile.clone(), wake, was_idle_dormant)
        };
        if let Ok(storage) = crate::session::Storage::new(&profile, self.file_watch.clone()) {
            let id_clone = id.to_string();
            let outcome = tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                        apply_prompt_persist_to_disk(inst, wake);
                    }
                    Ok(())
                })
            })
            .await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    target: "server.session_service",
                    session = %id,
                    "failed to save after prompt touch and wake: {e}"
                ),
                Err(join_err) => tracing::warn!(
                    target: "server.session_service",
                    session = %id,
                    "spawn_blocking join error during prompt touch save: {join_err}"
                ),
            }
        }
        PromptTouch::Touched {
            idle_dormant: woke_idle_dormant,
        }
    }

    /// Deliver a turn to a structured session.
    pub(crate) async fn send_turn(
        self: &Arc<Self>,
        caller: &SessionCaller,
        id: &str,
        turn: SendTurnRequest<'_>,
    ) -> Result<(), SendTurnError> {
        let SendTurnRequest {
            text,
            attachments,
            woke_idle_dormant,
            prompt_id,
            synthesized,
            no_revive,
        } = turn;
        use crate::server::acp_reconciler::ResumeTrigger;
        // Ownership gate, before ANY side effect (no wake, resume, publish, or forward for
        // a denied caller).
        let (acp_mode_id, yolo_mode) = {
            let instances = self.instances.read().await;
            let Some(inst) = instances.iter().find(|i| i.id == id) else {
                return Err(SendTurnError::SessionNotFound);
            };
            if let SessionCaller::Plugin { plugin_id } = caller {
                if inst.created_by_plugin.as_deref() != Some(plugin_id.as_str()) {
                    return Err(SendTurnError::NotOwner);
                }
            }
            (inst.acp_mode_id.clone(), inst.yolo_mode)
        };
        // Resume a worker that is not currently live.
        let needs_resume = woke_idle_dormant || !self.acp_supervisor.is_running(id).await;
        if no_revive && needs_resume {
            return Err(SendTurnError::RevivalRefused);
        }
        if needs_resume {
            match crate::server::acp_reconciler::trigger_resume_background(self, id).await {
                Ok(ResumeTrigger::NotFound) => return Err(SendTurnError::SessionNotFound),
                Ok(_) => {}
                Err(e) => return Err(SendTurnError::ResumeFailed(e)),
            }
        }
        // Gate the publish below on the worker actually being there.
        if let Err(e) = self.acp_supervisor.wait_until_ready(id).await {
            return match e {
                crate::acp::supervisor::SupervisorError::UnknownSession(_) => {
                    Err(SendTurnError::WorkerNotReady)
                }
                other => Err(SendTurnError::ResumeFailed(other)),
            };
        }
        // A plugin-delivered turn must run under the session's persisted explicit mode.
        if matches!(caller, SessionCaller::Plugin { .. }) {
            if let Some(mode_id) = &acp_mode_id {
                if let Err(e) = self.acp_supervisor.set_mode(id, mode_id).await {
                    return Err(SendTurnError::ModeApplication(e));
                }
            }
        }
        // Publish the user's prompt into the event stream BEFORE forwarding to the agent so
        // the replay buffer / on-disk store captures it even if the agent forward fails.
        let disposition = self
            .acp_supervisor
            .publish_user_prompt_with_attachments(
                id,
                text.to_string(),
                attachments,
                prompt_id,
                synthesized,
            )
            .await;
        let outcome = match disposition {
            crate::acp::supervisor::PromptDisposition::Forward => {
                self.acp_supervisor.send_prompt(id, text, attachments).await
            }
            crate::acp::supervisor::PromptDisposition::ResetContext => {
                self.acp_supervisor
                    .reset_session_context(id, text, acp_mode_id.as_deref(), yolo_mode)
                    .await
            }
        };
        match outcome {
            Ok(()) => Ok(()),
            // Intentional override of the canonical UnknownSession 404.
            Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) if needs_resume => {
                Err(SendTurnError::WorkerNotReady)
            }
            Err(e) => Err(SendTurnError::Send(e)),
        }
    }

    /// Deliver a session's persisted `pending_initial_turn`, then clear it.
    pub(crate) async fn drain_pending_initial_turn(self: &Arc<Self>, id: &str) {
        {
            let mut drains = self
                .pending_drains
                .lock()
                .expect("pending_drains mutex poisoned");
            if !drains.insert(id.to_string()) {
                return;
            }
        }
        let _claim = PendingDrainGuard {
            service: Arc::clone(self),
            id: id.to_string(),
        };
        // Non-vivifying.
        let Some(_submission) = self.prompt_submission_for_session(id).await else {
            return;
        };
        let Some((text, attachment_refs, synthesized, profile, caller)) = ({
            let instances = self.instances.read().await;
            instances.iter().find(|i| i.id == id).and_then(|i| {
                i.pending_initial_turn.clone().map(|turn| {
                    // Reconstruct the creator principal so plugin-created
                    // pending turns keep plugin attribution and the plugin
                    // mode-assertion path; user-created ones stay User.
                    let caller = match &i.created_by_plugin {
                        Some(plugin_id) => SessionCaller::Plugin {
                            plugin_id: plugin_id.clone(),
                        },
                        None => SessionCaller::User,
                    };
                    (
                        turn.text,
                        turn.attachments,
                        turn.synthesized,
                        i.source_profile.clone(),
                        caller,
                    )
                })
            })
        }) else {
            return;
        };
        // Reload the attachment blobs so a rate-limit resume continuation replays the
        // interrupted prompt's images/files, not just its text.
        let attachments = if attachment_refs.is_empty() {
            Vec::new()
        } else {
            let store = Arc::clone(&self.acp_event_store);
            let id_load = id.to_string();
            tokio::task::spawn_blocking(move || {
                attachment_refs
                    .into_iter()
                    .filter_map(|r| {
                        store
                            .load_attachment(&id_load, &r.id)
                            .map(
                                |(mime_type, data)| crate::acp::event_store::AttachmentBlob {
                                    id: r.id,
                                    kind: r.kind,
                                    mime_type,
                                    name: r.name,
                                    data,
                                },
                            )
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default()
        };
        if let Err(e) = self
            .send_turn(
                &caller,
                id,
                SendTurnRequest {
                    text: &text,
                    attachments: &attachments,
                    woke_idle_dormant: false,
                    prompt_id: None,
                    synthesized,
                    no_revive: false,
                },
            )
            .await
        {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "pending initial turn delivery failed; the reconciler will retry: {e}"
            );
            return;
        }
        {
            let mut instances = self.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.pending_initial_turn = None;
                self.invalidate_disk_snapshots();
            }
        }
        match crate::session::Storage::new(&profile, self.file_watch.clone()) {
            Ok(storage) => {
                let id_persist = id.to_string();
                let persisted = tokio::task::spawn_blocking(move || {
                    storage.update(|instances, _groups| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                            inst.pending_initial_turn = None;
                        }
                        Ok(())
                    })
                })
                .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %id,
                        "failed to persist pending initial turn clear; a daemon restart re-delivers it"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    "failed to open storage to clear pending initial turn: {e}"
                );
            }
        }
    }

    /// Queue `text` (with its `attachments` refs) as the session's next turn, reusing the
    /// pending-initial-turn drain so the turn is delivered once the (resumed) worker is
    /// live.
    pub(crate) async fn set_pending_initial_turn(
        self: &Arc<Self>,
        id: &str,
        text: String,
        attachments: Vec<crate::daemon::PromptAttachmentRef>,
    ) {
        let turn = crate::session::PendingInitialTurn {
            text,
            attachments,
            synthesized: true,
        };
        let profile = {
            let mut instances = self.instances.write().await;
            match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) if inst.pending_initial_turn.is_none() => {
                    inst.pending_initial_turn = Some(turn.clone());
                    self.invalidate_disk_snapshots();
                    inst.source_profile.clone()
                }
                _ => return,
            }
        };
        match crate::session::Storage::new(&profile, self.file_watch.clone()) {
            Ok(storage) => {
                let id_persist = id.to_string();
                let persisted = tokio::task::spawn_blocking(move || {
                    storage.update(|instances, _groups| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                            inst.pending_initial_turn = Some(turn);
                        }
                        Ok(())
                    })
                })
                .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %id,
                        "failed to persist resume continuation turn; it still drains this daemon life"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    "failed to open storage for resume continuation turn: {e}"
                );
            }
        }
    }

    /// Drop any queued pending initial turn (text + attachment refs) for a session, in
    /// memory and on disk.
    pub(crate) async fn clear_pending_initial_turn(self: &Arc<Self>, id: &str) {
        let profile = {
            let mut instances = self.instances.write().await;
            match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) if inst.pending_initial_turn.is_some() => {
                    inst.pending_initial_turn = None;
                    self.invalidate_disk_snapshots();
                    inst.source_profile.clone()
                }
                _ => return,
            }
        };
        match crate::session::Storage::new(&profile, self.file_watch.clone()) {
            Ok(storage) => {
                let id_persist = id.to_string();
                let persisted = tokio::task::spawn_blocking(move || {
                    storage.update(|instances, _groups| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                            inst.pending_initial_turn = None;
                        }
                        Ok(())
                    })
                })
                .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %id,
                        "failed to persist pending-turn clear; the drain re-checks liveness before delivery"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    "failed to open storage to clear pending turn: {e}"
                );
            }
        }
    }

    /// Drop any disk reload that read `sessions.json` before this in-memory change. Call
    /// under the `instances` write lock; the persist that follows schedules a fresh reload.
    fn invalidate_disk_snapshots(&self) {
        self.mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Apply `mutate` to a session's in-memory `Instance`, then mirror the resulting state
    /// to disk.
    async fn mutate_instance_persisted<T, F>(self: &Arc<Self>, id: &str, mutate: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut crate::session::Instance) -> T,
    {
        let persist_lock = self.persist_lock(id).await;
        let _ordered = persist_lock.lock().await;
        let (profile, result, mirrored) = {
            let mut instances = self.instances.write().await;
            let inst = instances.iter_mut().find(|i| i.id == id)?;
            let r = mutate(inst);
            self.invalidate_disk_snapshots();
            (
                inst.source_profile.clone(),
                r,
                MirroredFields {
                    queued_prompts: inst.queued_prompts.clone(),
                    queued_prompt_next_seq: inst.queued_prompt_next_seq,
                    idle_dormant_since: inst.idle_dormant_since,
                    last_accessed_at: inst.last_accessed_at,
                },
            )
        };
        match crate::session::Storage::new(&profile, self.file_watch.clone()) {
            Ok(storage) => {
                let id_persist = id.to_string();
                let persisted = tokio::task::spawn_blocking(move || {
                    storage.update(|instances, _groups| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                            inst.queued_prompts = mirrored.queued_prompts;
                            inst.queued_prompt_next_seq = mirrored.queued_prompt_next_seq;
                            inst.idle_dormant_since = mirrored.idle_dormant_since;
                            // Monotone max, never `touch_last_accessed()`.
                            inst.last_accessed_at =
                                inst.last_accessed_at.max(mirrored.last_accessed_at);
                        }
                        Ok(())
                    })
                })
                .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(target: "acp.queue", session = %id, "failed to persist queue mutation; it holds this daemon life");
                }
            }
            Err(e) => {
                tracing::warn!(target: "acp.queue", session = %id, "failed to open storage for queue mutation: {e}");
            }
        }
        Some(result)
    }

    /// Append a prompt to the session's server-owned queue and return the stored entry
    /// (with its assigned `seq`).
    pub(crate) async fn enqueue_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
        text: String,
        attachments: Vec<crate::daemon::PromptAttachmentRef>,
        origin_device: Option<String>,
        created_at: String,
    ) -> Option<crate::daemon::QueuedPromptEntry> {
        self.mutate_instance_persisted(id, move |inst| {
            inst.last_accessed_at = inst.last_accessed_at.max(Some(chrono::Utc::now()));
            if let Some(existing) = inst.queued_prompts.iter_mut().find(|q| q.id == prompt_id) {
                existing.text = text.clone();
                existing.attachments = attachments.clone();
                return existing.clone();
            }
            let seq = inst.queued_prompt_next_seq;
            inst.queued_prompt_next_seq = seq.saturating_add(1);
            let entry = crate::daemon::QueuedPromptEntry {
                id: prompt_id.clone(),
                seq,
                text: text.clone(),
                attachments: attachments.clone(),
                created_at: created_at.clone(),
                origin_device: origin_device.clone(),
            };
            inst.queued_prompts.push(entry.clone());
            entry
        })
        .await
    }

    /// Replace a queued prompt's text in place.
    pub(crate) async fn edit_queued_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
        text: String,
    ) -> EditQueuedOutcome {
        let Some(_submission) = self.prompt_submission_for_session(id).await else {
            return EditQueuedOutcome::NotFound;
        };
        self.mutate_instance_persisted(id, move |inst| {
            match inst.queued_prompts.iter_mut().find(|q| q.id == prompt_id) {
                Some(q) if text.trim().is_empty() && q.attachments.is_empty() => {
                    EditQueuedOutcome::WouldEmpty
                }
                Some(q) => {
                    q.text = text.clone();
                    EditQueuedOutcome::Updated
                }
                None => EditQueuedOutcome::NotFound,
            }
        })
        .await
        .unwrap_or(EditQueuedOutcome::NotFound)
    }

    /// Remove a queued prompt by id.
    pub(crate) async fn remove_queued_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
    ) -> bool {
        let Some(_submission) = self.prompt_submission_for_session(id).await else {
            return false;
        };
        let prompt_id_cleanup = prompt_id.clone();
        let removed = self
            .mutate_instance_persisted(id, move |inst| {
                let before = inst.queued_prompts.len();
                inst.queued_prompts.retain(|q| q.id != prompt_id);
                inst.queued_prompts.len() != before
            })
            .await
            .unwrap_or(false);
        if removed {
            self.acp_event_store
                .delete_pending_attachments_for_ref(id, &prompt_id_cleanup);
        }
        removed
    }

    /// Drop every queued prompt for a session, plus every attachment blob buffered for
    /// those prompts.
    pub(crate) async fn clear_queued_prompts(self: &Arc<Self>, id: &str) {
        let Some(_submission) = self.prompt_submission_for_session(id).await else {
            return;
        };
        let cleared_ids = self
            .mutate_instance_persisted(id, move |inst| {
                let ids: Vec<String> = inst.queued_prompts.iter().map(|q| q.id.clone()).collect();
                inst.queued_prompts.clear();
                ids
            })
            .await
            .unwrap_or_default();
        for prompt_id in cleared_ids {
            self.acp_event_store
                .delete_pending_attachments_for_ref(id, &prompt_id);
        }
    }

    /// Snapshot the session's queue, ordered by `seq`.
    pub(crate) async fn queued_prompts_snapshot(
        &self,
        id: &str,
    ) -> Vec<crate::daemon::QueuedPromptEntry> {
        let instances = self.instances.read().await;
        instances
            .iter()
            .find(|i| i.id == id)
            .map(|i| {
                let mut q = i.queued_prompts.clone();
                q.sort_by_key(|e| e.seq);
                q
            })
            .unwrap_or_default()
    }

    /// The daemon's live control state for a session, folded once at the publish choke
    /// point and hydrated from the event log on a cache miss.
    pub(crate) async fn fold_control_state(&self, id: &str) -> crate::acp::state::AcpState {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName};
        let (agent, model) = {
            let instances = self.instances.read().await;
            instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| {
                    (
                        AgentName(i.agent_name.clone().unwrap_or_else(|| i.tool.clone())),
                        i.agent_model.clone(),
                    )
                })
                .unwrap_or_else(|| (AgentName(String::new()), None))
        };
        let store = Arc::clone(&self.acp_event_store);
        let cache = Arc::clone(&self.acp_control_cache);
        let sid = id.to_string();
        // The hydrate closure runs under the cache's per-session lock and does a locking
        // SQLite scan, so the whole thing goes off the runtime rather than just the scan.
        tokio::task::spawn_blocking(move || {
            cache.get_or_hydrate(&sid.clone(), || {
                let mut reduced = AcpState::new(AcpSessionId(sid.clone()), agent, model);
                let mut last_seq = 0;
                for (seq, event) in store.replay_from(&sid, 0) {
                    let _ = reduced.apply_event(event);
                    last_seq = seq;
                }
                (reduced, last_seq)
            })
        })
        .await
        .unwrap_or_else(|_| {
            // The blocking pool panicked or shut down.
            let mut fallback =
                AcpState::new(AcpSessionId(id.to_string()), AgentName(String::new()), None);
            fallback.turn_active = true;
            fallback
        })
    }

    /// Drain the leading batch of a session's server-owned queue into the live worker once
    /// the current turn has ended.
    pub(crate) async fn drain_queued_prompts_once(self: &Arc<Self>, id: &str) {
        {
            let mut drains = self
                .pending_drains
                .lock()
                .expect("pending_drains mutex poisoned");
            if !drains.insert(id.to_string()) {
                return;
            }
        }
        let _claim = PendingDrainGuard {
            service: Arc::clone(self),
            id: id.to_string(),
        };
        // Non-vivifying, for the same reason as the pending-initial drain.
        let Some(_submission) = self.prompt_submission_for_session(id).await else {
            return;
        };

        let (caller, agent_key, queue) = {
            let instances = self.instances.read().await;
            let Some(inst) = instances.iter().find(|i| i.id == id) else {
                return;
            };
            if !inst.is_structured()
                || inst.is_archived()
                || inst.is_snoozed()
                || inst.is_trashed()
                || inst.status != crate::session::Status::Idle
            {
                return;
            }
            let mut queue = inst.queued_prompts.clone();
            queue.sort_by_key(|e| e.seq);
            if queue.is_empty() {
                return;
            }
            let caller = match &inst.created_by_plugin {
                Some(plugin_id) => SessionCaller::Plugin {
                    plugin_id: plugin_id.clone(),
                },
                None => SessionCaller::User,
            };
            let agent_key = inst.agent_name.clone().unwrap_or_else(|| inst.tool.clone());
            (caller, agent_key, queue)
        };

        // `Status::Idle` above is a mirror the broadcast listener applies one serial task
        // behind every session's events, so it still reads Idle for as long as that task is
        // behind.
        if self.fold_control_state(id).await.turn_active {
            return;
        }

        // Leading batch up to a clear boundary (mirrors the client's split).
        let profile = crate::acp::agent_profiles::resolve(&agent_key);
        let (sub, combined) = queue_drain_batch(&queue, profile);
        let sent_ids: Vec<String> = sub.iter().map(|e| e.id.clone()).collect();
        // Reload every buffered attachment blob for the batch, in queue order, so a queued
        // screenshot/file is forwarded with the text (matches the client's old
        // `snapshot.flatMap(q => q.attachments)`). Bytes live in the pending-attachment
        // store keyed by prompt id; a locking SQLite read, so do it off the runtime.
        let attachments: Vec<crate::acp::event_store::AttachmentBlob> = {
            let store = Arc::clone(&self.acp_event_store);
            let session_id = id.to_string();
            let batch_ids = sent_ids.clone();
            tokio::task::spawn_blocking(move || {
                batch_ids
                    .iter()
                    .flat_map(|pid| store.load_pending_attachments_for_ref(&session_id, pid))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default()
        };
        // An attachment-only batch has empty combined text but real blobs.
        if combined.trim().is_empty() && attachments.is_empty() {
            tracing::warn!(
                target: "acp.queue",
                session = %id,
                rows = sent_ids.len(),
                "queued prompts have neither text nor attachment bytes (buffered bytes expired?); \
                 retiring them so the queue behind them can drain"
            );
            self.retire_drained_rows(id, sent_ids).await;
            return;
        }

        // Deliver as a fresh turn on the live worker.
        if let Err(e) = self
            .send_turn(
                &caller,
                id,
                SendTurnRequest {
                    text: &combined,
                    attachments: &attachments,
                    woke_idle_dormant: false,
                    prompt_id: None,
                    synthesized: false,
                    no_revive: false,
                },
            )
            .await
        {
            tracing::warn!(target: "acp.queue", session = %id, "queue drain delivery failed; will retry: {e}");
            return;
        }
        // Retire only the delivered rows; prompts enqueued during the send
        // survive into the next drain.
        self.retire_drained_rows(id, sent_ids).await;
    }

    /// Drop a set of queue rows and the attachment bytes buffered for them.
    async fn retire_drained_rows(self: &Arc<Self>, id: &str, ids: Vec<String>) {
        for pid in &ids {
            self.acp_event_store
                .delete_pending_attachments_for_ref(id, pid);
        }
        let retire: std::collections::HashSet<String> = ids.into_iter().collect();
        self.mutate_instance_persisted(id, move |inst| {
            inst.queued_prompts.retain(|q| !retire.contains(&q.id));
        })
        .await;
    }

    /// Clear the idle-dormant marker for a session that has queued work so the reconciler's
    /// resume pass respawns its worker, after which the next tick drains the queue (see
    /// `acp_reconciler::drain_queued_prompts`).
    pub(crate) async fn wake_dormant_for_queue_drain(self: &Arc<Self>, id: &str) {
        self.mutate_instance_persisted(id, |inst| {
            if inst.is_idle_dormant() {
                inst.idle_dormant_since = None;
            }
        })
        .await;
    }

    /// Same lazy per-instance mutex registry as `AppState::instance_lock`; both operate on
    /// the shared map, so a lock taken through either handle excludes the other.
    async fn persist_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.persist_locks.read().await;
            if let Some(lock) = guard.get(id) {
                return lock.clone();
            }
        }
        let mut guard = self.persist_locks.write().await;
        guard
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.instance_locks.read().await;
            if let Some(lock) = guard.get(id) {
                return lock.clone();
            }
        }
        let mut guard = self.instance_locks.write().await;
        guard
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// The session's single prompt-submission authority.
    pub(crate) async fn prompt_submission(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        #[cfg(test)]
        if let Some(tap) = self.submission_claims.get() {
            let _ = tap.send(id.to_string());
        }
        let lock = {
            let guard = self.prompt_locks.read().await;
            guard.get(id).cloned()
        };
        let lock = match lock {
            Some(lock) => lock,
            None => self
                .prompt_locks
                .write()
                .await
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone(),
        };
        lock.lock_owned().await
    }

    /// [`Self::prompt_submission`] for a caller that has not yet proved it may act on the
    /// session.
    pub(crate) async fn admit_prompt_submission(
        &self,
        caller: &SessionCaller,
        id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, TurnAdmissionError> {
        self.admits_turn(caller, id).await?;
        let guard = self.prompt_submission(id).await;
        if let Err(e) = self.admits_turn(caller, id).await {
            drop(guard);
            // Only for a vanished session.
            if matches!(e, TurnAdmissionError::SessionNotFound) {
                self.forget_prompt_lock(id).await;
            }
            return Err(e);
        }
        Ok(guard)
    }

    /// May `caller` open a turn on `id`?
    pub(crate) async fn admits_turn(
        &self,
        caller: &SessionCaller,
        id: &str,
    ) -> Result<(), TurnAdmissionError> {
        let instances = self.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return Err(TurnAdmissionError::SessionNotFound);
        };
        match caller {
            SessionCaller::User => Ok(()),
            SessionCaller::Plugin { plugin_id } => {
                if inst.created_by_plugin.as_deref() == Some(plugin_id.as_str()) {
                    Ok(())
                } else {
                    Err(TurnAdmissionError::NotOwner)
                }
            }
        }
    }

    /// [`Self::admit_prompt_submission`] for a user surface, which only ever
    /// fails on a session that no longer exists.
    pub(crate) async fn prompt_submission_for_session(
        &self,
        id: &str,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.admit_prompt_submission(&SessionCaller::User, id)
            .await
            .ok()
    }

    /// Settle the prompt's disposition under a submission guard the caller already holds,
    /// so every turn-starting surface decides and dispatches as one step instead of
    /// dispatching unconditionally after the wait.
    pub(crate) async fn prompt_dispatch_under_submission(
        &self,
        id: &str,
        idle_dormant: bool,
    ) -> crate::acp::dispatch::PromptDispatch {
        let running = self.acp_supervisor.is_running(id).await;
        // Settled here, under the guard, rather than probed by each handler before it
        // claims one.
        let rate_limit_parked = !running && !idle_dormant && self.is_rate_limit_parked(id).await;
        let liveness = crate::acp::dispatch::WorkerLiveness {
            running,
            idle_dormant,
            rate_limit_parked,
        };
        crate::acp::dispatch::decide(&self.fold_control_state(id).await, liveness)
    }

    /// See [`crate::acp::dispatch::WorkerLiveness::rate_limit_parked`].
    async fn is_rate_limit_parked(&self, id: &str) -> bool {
        let store = Arc::clone(&self.acp_event_store);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.rate_limit_park(&id).is_some())
            .await
            .unwrap_or(false)
    }

    /// Drop a deleted session's submission lock, mirroring the `instance_locks` removal the
    /// same delete paths already do.
    pub(crate) async fn forget_prompt_lock(&self, id: &str) {
        self.prompt_locks.write().await.remove(id);
    }

    /// Registry size for a test asserting `prompt_locks` stays bounded (e.g.
    #[cfg(test)]
    pub(crate) async fn prompt_locks_len(&self) -> usize {
        self.prompt_locks.read().await.len()
    }

    /// Report every [`Self::prompt_submission`] claim at the one moment a deletion-race
    /// test can use.
    #[cfg(test)]
    pub(crate) fn watch_submission_claims(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.submission_claims
            .set(tx)
            .expect("one submission watcher per service");
        rx
    }
}

/// Releases a session's `pending_drains` claim on every exit path of
/// [`SessionService::drain_pending_initial_turn`], including panics.
struct PendingDrainGuard {
    service: Arc<SessionService>,
    id: String,
}

impl Drop for PendingDrainGuard {
    fn drop(&mut self) {
        self.service
            .pending_drains
            .lock()
            .expect("pending_drains mutex poisoned")
            .remove(&self.id);
    }
}

/// Releases the in-flight slot and wakes waiters on every exit path of the winning create,
/// including an error return or a panic unwinding through the caller.
struct InFlightGuard {
    service: Arc<SessionService>,
    scope: (String, String),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = self
            .service
            .create_in_flight
            .lock()
            .expect("create_in_flight mutex poisoned");
        if let Some(entry) = in_flight.remove(&self.scope) {
            entry.notify.notify_waiters();
        }
    }
}

/// Match a plugin create request against the persisted sessions by `(created_by_plugin,
/// idempotency key)`.
fn find_idempotent_match(
    instances: &[Instance],
    plugin_id: &str,
    key: &str,
    payload_hash: &str,
) -> IdempotentMatch {
    for instance in instances {
        if instance.created_by_plugin.as_deref() != Some(plugin_id) {
            continue;
        }
        let Some(record) = &instance.plugin_create_idempotency else {
            continue;
        };
        if record.key != key {
            continue;
        }
        if record.payload_hash == payload_hash {
            return IdempotentMatch::Same(Box::new(instance.clone()));
        }
        return IdempotentMatch::Conflict;
    }
    IdempotentMatch::None
}

/// Versioned, restart-stable hash of the semantic create request.
fn spec_payload_hash(spec: &StructuredSessionSpec) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut field = |name: &str, value: &str| {
        hasher.update(name.as_bytes());
        hasher.update([0x1f]);
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
        hasher.update([0x1e]);
    };
    field("version", "1");
    field("title", spec.title.as_deref().unwrap_or_default());
    field("path", &spec.path);
    field("group", &spec.group);
    field("tool", &spec.tool);
    field("worktree_enabled", &spec.worktree_enabled.to_string());
    field(
        "worktree_branch",
        spec.worktree_branch.as_deref().unwrap_or_default(),
    );
    field("create_new_branch", &spec.create_new_branch.to_string());
    field(
        "base_branch",
        spec.base_branch.as_deref().unwrap_or_default(),
    );
    field("sandbox", &spec.sandbox.to_string());
    field(
        "sandbox_image",
        spec.sandbox_image.as_deref().unwrap_or_default(),
    );
    field("yolo_mode", &spec.yolo_mode.to_string());
    field("extra_env", &spec.extra_env.join("\x1f"));
    field("extra_args", &spec.extra_args);
    field("command_override", &spec.command_override);
    field("extra_repo_paths", &spec.extra_repo_paths.join("\x1f"));
    field("scratch", &spec.scratch.to_string());
    field(
        "custom_instruction",
        spec.custom_instruction.as_deref().unwrap_or_default(),
    );
    field("profile", &spec.profile);
    field(
        "initial_turn",
        spec.pending_initial_turn.as_deref().unwrap_or_default(),
    );
    field(
        "acp_mode_id",
        spec.acp_mode_id.as_deref().unwrap_or_default(),
    );
    field("view", &format!("{:?}", spec.view));
    field("agent_name", spec.agent_name.as_deref().unwrap_or_default());
    field(
        "agent_model",
        spec.agent_model.as_deref().unwrap_or_default(),
    );
    field(
        "agent_effort",
        spec.agent_effort.as_deref().unwrap_or_default(),
    );
    field(
        "import_acp_session_id",
        spec.import_acp_session_id.as_deref().unwrap_or_default(),
    );
    use std::fmt::Write;
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Disk-side half of [`SessionService::touch_and_wake_on_prompt`], mirroring onto disk
/// whatever the memory side actually did.
pub(crate) fn apply_prompt_persist_to_disk(disk: &mut crate::session::Instance, wake: bool) {
    if wake {
        disk.touch_last_accessed();
    } else {
        let now = chrono::Utc::now();
        disk.last_accessed_at = disk.last_accessed_at.max(Some(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_for(rows: Vec<Instance>) -> Arc<SessionService> {
        crate::server::test_support::build_test_app_state(rows)
            .session_service
            .clone()
    }

    fn plugin_instance(plugin_id: &str, key: &str, payload_hash: &str) -> Instance {
        let mut inst = Instance::new("scheduled", "/tmp/aoe-2897-project");
        inst.created_by_plugin = Some(plugin_id.to_string());
        inst.plugin_create_idempotency = Some(PluginCreateIdempotency {
            key: key.to_string(),
            payload_hash: payload_hash.to_string(),
        });
        inst
    }

    fn test_spec() -> StructuredSessionSpec {
        StructuredSessionSpec {
            title: Some("nightly".to_string()),
            path: "/tmp/aoe-2897-project".to_string(),
            group: String::new(),
            tool: "claude".to_string(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: None,
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: Vec::new(),
            repo_base_branches: Vec::new(),
            scratch: false,
            trust_hooks: None,
            custom_instruction: None,
            callback_url: None,
            idempotency_key: None,
            profile: "default".to_string(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
            pending_initial_turn: None,
            acp_mode_id: None,
            view: crate::session::View::Structured,
            agent_name: Some("claude".to_string()),
            agent_model: None,
            agent_effort: None,
            import_acp_session_id: None,
            fork_seed: None,
        }
    }

    #[test]
    fn payload_hash_is_deterministic_and_field_sensitive() {
        let spec = test_spec();
        let a = spec_payload_hash(&spec);
        let b = spec_payload_hash(&test_spec());
        assert_eq!(a, b, "same spec must hash identically across calls");

        let mut changed = test_spec();
        changed.path = "/tmp/aoe-2897-other".to_string();
        assert_ne!(
            a,
            spec_payload_hash(&changed),
            "a semantic field change must change the hash"
        );

        let mut with_turn = test_spec();
        with_turn.pending_initial_turn = Some("run the nightly task".to_string());
        assert_ne!(
            a,
            spec_payload_hash(&with_turn),
            "the initial turn is part of the request identity"
        );

        // Adjacent-field concatenation must not collide.
        let mut shifted_a = test_spec();
        shifted_a.extra_args = "ab".to_string();
        shifted_a.command_override = "c".to_string();
        let mut shifted_b = test_spec();
        shifted_b.extra_args = "a".to_string();
        shifted_b.command_override = "bc".to_string();
        assert_ne!(spec_payload_hash(&shifted_a), spec_payload_hash(&shifted_b));
    }

    #[test]
    fn idempotent_match_same_conflict_and_scope() {
        let instances = vec![plugin_instance("cron", "job-1:2026-07-16", "hash-a")];

        assert!(matches!(
            find_idempotent_match(&instances, "cron", "job-1:2026-07-16", "hash-a"),
            IdempotentMatch::Same(_)
        ));
        assert!(matches!(
            find_idempotent_match(&instances, "cron", "job-1:2026-07-16", "hash-b"),
            IdempotentMatch::Conflict
        ));
        // Another plugin may reuse the same key: scopes are per plugin id.
        assert!(matches!(
            find_idempotent_match(&instances, "other-plugin", "job-1:2026-07-16", "hash-a"),
            IdempotentMatch::None
        ));
        assert!(matches!(
            find_idempotent_match(&instances, "cron", "job-2:2026-07-16", "hash-a"),
            IdempotentMatch::None
        ));
    }

    #[test]
    fn idempotent_match_survives_triage_but_not_removal() {
        let mut archived = plugin_instance("cron", "k", "h");
        archived.archived_at = Some(chrono::Utc::now());
        let mut trashed = plugin_instance("cron", "k2", "h");
        trashed.trashed_at = Some(chrono::Utc::now());
        let instances = vec![archived, trashed];

        assert!(matches!(
            find_idempotent_match(&instances, "cron", "k", "h"),
            IdempotentMatch::Same(_)
        ));
        assert!(matches!(
            find_idempotent_match(&instances, "cron", "k2", "h"),
            IdempotentMatch::Same(_)
        ));
        // Hard delete: the record is gone from the list, the key is free.
        assert!(matches!(
            find_idempotent_match(&[], "cron", "k", "h"),
            IdempotentMatch::None
        ));
    }

    #[tokio::test]
    async fn in_flight_claim_waits_same_hash_and_conflicts_on_mismatch() {
        let service = service_for(Vec::new());
        let scope = ("cron".to_string(), "job-1".to_string());

        let ClaimOutcome::Claimed = service.try_claim_in_flight(&scope, "hash-a") else {
            panic!("first claim must win");
        };
        let ClaimOutcome::Wait(notify) = service.try_claim_in_flight(&scope, "hash-a") else {
            panic!("identical concurrent claim must wait");
        };
        let ClaimOutcome::Conflict = service.try_claim_in_flight(&scope, "hash-b") else {
            panic!("same key with a different payload must conflict");
        };

        let notified = notify.notified();
        tokio::pin!(notified);
        assert!(futures_util::poll!(&mut notified).is_pending());
        drop(InFlightGuard {
            service: Arc::clone(&service),
            scope: scope.clone(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("guard drop must wake waiters");

        let ClaimOutcome::Claimed = service.try_claim_in_flight(&scope, "hash-a") else {
            panic!("released scope must be claimable again");
        };
    }

    #[tokio::test]
    async fn probe_resolves_replay_conflict_and_new() {
        // Seed a prior create whose stored hash matches `test_spec()`; the probe
        // must resolve replay/conflict from the persisted list alone, so a
        // caller can skip admission (rate/concurrency) for an idempotent retry.
        let spec = test_spec();
        let hash = spec_payload_hash(&spec);
        let mut prior = plugin_instance("cron", "job-1", &hash);
        prior.id = "sess-prior".to_string();
        let service = service_for(vec![prior]);

        // Same plugin, key, and payload: replay the existing session.
        match service
            .probe_plugin_create_idempotency(&spec, "cron", "job-1")
            .await
        {
            Ok(CreateIdempotencyProbe::Replay(inst)) => assert_eq!(inst.id, "sess-prior"),
            _ => panic!("expected replay"),
        }

        // Same plugin and key, different payload: conflict.
        let mut other = test_spec();
        other.title = Some("different".to_string());
        assert!(service
            .probe_plugin_create_idempotency(&other, "cron", "job-1")
            .await
            .is_err());

        // Unknown key: a genuinely new create.
        assert!(matches!(
            service
                .probe_plugin_create_idempotency(&spec, "cron", "job-2")
                .await,
            Ok(CreateIdempotencyProbe::New)
        ));

        // Another plugin's session with the same key: new (never cross-plugin).
        assert!(matches!(
            service
                .probe_plugin_create_idempotency(&spec, "other-plugin", "job-1")
                .await,
            Ok(CreateIdempotencyProbe::New)
        ));
    }

    #[tokio::test]
    async fn send_turn_enforces_plugin_ownership_before_any_side_effect() {
        let mut user_session = Instance::new("user-owned", "/tmp/aoe-2897-project");
        user_session.id = "sess-user".to_string();
        let mut cron_session = Instance::new("cron-owned", "/tmp/aoe-2897-project");
        cron_session.id = "sess-cron".to_string();
        cron_session.created_by_plugin = Some("cron".to_string());
        let service = service_for(vec![user_session, cron_session]);

        let cron = SessionCaller::Plugin {
            plugin_id: "cron".to_string(),
        };
        let other = SessionCaller::Plugin {
            plugin_id: "other-plugin".to_string(),
        };

        // A plugin cannot deliver to a user-created session, another
        // plugin's session, or a missing session.
        assert!(matches!(
            service
                .send_turn(
                    &cron,
                    "sess-user",
                    SendTurnRequest {
                        text: "hi",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await,
            Err(SendTurnError::NotOwner)
        ));
        assert!(matches!(
            service
                .send_turn(
                    &other,
                    "sess-cron",
                    SendTurnRequest {
                        text: "hi",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await,
            Err(SendTurnError::NotOwner)
        ));
        assert!(matches!(
            service
                .send_turn(
                    &cron,
                    "sess-gone",
                    SendTurnRequest {
                        text: "hi",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await,
            Err(SendTurnError::SessionNotFound)
        ));

        // The owner passes the gate; these terminal-view test sessions fail at a LATER
        // stage (resume snapshot or worker capacity, both environment dependent), proving
        // the denials above came from the ownership check specifically.
        assert!(!matches!(
            service
                .send_turn(
                    &cron,
                    "sess-cron",
                    SendTurnRequest {
                        text: "hi",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await,
            Ok(()) | Err(SendTurnError::NotOwner)
        ));
        assert!(!matches!(
            service
                .send_turn(
                    &SessionCaller::User,
                    "sess-user",
                    SendTurnRequest {
                        text: "hi",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await,
            Ok(()) | Err(SendTurnError::NotOwner)
        ));
    }

    #[tokio::test]
    async fn drain_is_a_noop_without_a_pending_turn_and_releases_its_claim() {
        let mut inst = Instance::new("no-pending", "/tmp/aoe-2897-project");
        inst.id = "sess-drain".to_string();
        inst.view = crate::session::View::Structured;
        let service = service_for(vec![inst]);

        // No pending turn.
        service.drain_pending_initial_turn("sess-drain").await;
        service.drain_pending_initial_turn("sess-missing").await;
        assert!(
            service
                .pending_drains
                .lock()
                .expect("pending_drains mutex poisoned")
                .is_empty(),
            "drain must release its claim on the no-op paths"
        );
    }

    /// A queued prompt whose buffered attachment bytes have gone (the 24h
    /// `PENDING_ATTACHMENT_TTL` sweep reclaims them; the row is not swept with them) has
    /// neither text nor blobs, so the drain can never deliver it.
    #[tokio::test]
    async fn an_undeliverable_queue_row_is_retired_instead_of_wedging_the_queue() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("queue", "/tmp/aoe-queue-husk");
        inst.id = "sess-husk".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let service = service_for(vec![inst]);

        // An attachment-only prompt.
        service
            .enqueue_prompt(
                "sess-husk",
                "husk".into(),
                String::new(),
                vec![crate::daemon::PromptAttachmentRef {
                    id: "att-1".into(),
                    kind: crate::daemon::PromptAttachmentKind::Image,
                    mime_type: "image/png".into(),
                    name: Some("shot.png".into()),
                    size: 9,
                }],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");
        assert_eq!(service.queued_prompts_snapshot("sess-husk").await.len(), 1);

        // The husk has to be the whole batch to wedge.
        service.drain_queued_prompts_once("sess-husk").await;
        assert!(
            service
                .queued_prompts_snapshot("sess-husk")
                .await
                .is_empty(),
            "the husk is retired rather than retried forever"
        );

        // And the queue is genuinely usable again, not just empty.
        service
            .enqueue_prompt(
                "sess-husk",
                "next".into(),
                "still deliverable".into(),
                vec![],
                None,
                "t1".into(),
            )
            .await
            .expect("session exists");
        assert_eq!(
            service
                .queued_prompts_snapshot("sess-husk")
                .await
                .iter()
                .map(|q| q.id.clone())
                .collect::<Vec<_>>(),
            ["next"]
        );
    }

    /// The drain must not deliver into a turn prompt dispatch parked the prompt behind.
    #[tokio::test]
    async fn a_queued_prompt_is_not_drained_into_a_turn_status_has_not_caught_up_with() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        use crate::acp::state::Event;
        use crate::acp::supervisor::BroadcastSink;

        let mut idle = Instance::new("idle", "/tmp/aoe-queue-idle");
        idle.id = "sess-idle".to_string();
        idle.view = crate::session::View::Structured;
        idle.status = crate::session::Status::Idle;
        let mut mid_turn = Instance::new("mid", "/tmp/aoe-queue-mid-turn");
        mid_turn.id = "sess-mid-turn".to_string();
        mid_turn.view = crate::session::View::Structured;
        // The lagging mirror.
        mid_turn.status = crate::session::Status::Idle;
        let state = crate::server::test_support::build_test_app_state(vec![idle, mid_turn]);
        let service = state.session_service.clone();

        // Publish through the real choke point.
        let sink = crate::acp::supervisor::ChannelSink {
            tx: state.acp_events_tx.clone(),
            event_store: Arc::clone(&state.acp_event_store),
            control_cache: Arc::clone(&state.acp_control_cache),
        };
        assert!(
            sink.publish_persisted(
                "sess-mid-turn",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    synthesized: false,
                },
            ),
            "publish must reach the event store"
        );

        for id in ["sess-idle", "sess-mid-turn"] {
            service
                .enqueue_prompt(
                    id,
                    "husk".into(),
                    String::new(),
                    vec![crate::daemon::PromptAttachmentRef {
                        id: "att-1".into(),
                        kind: crate::daemon::PromptAttachmentKind::Image,
                        mime_type: "image/png".into(),
                        name: Some("shot.png".into()),
                        size: 9,
                    }],
                    None,
                    "t0".into(),
                )
                .await
                .expect("session exists");
            service.drain_queued_prompts_once(id).await;
        }

        assert!(
            service
                .queued_prompts_snapshot("sess-idle")
                .await
                .is_empty(),
            "no turn in flight: the drain runs and retires the husk"
        );
        assert_eq!(
            service.queued_prompts_snapshot("sess-mid-turn").await.len(),
            1,
            "a turn is in flight, so the drain must leave the queue for the next tick"
        );
    }

    /// #3621.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_queue_drain_frees_instance_lock_while_it_waits_for_a_resuming_worker() {
        use crate::acp::supervisor::{ResumeKind, ResumeReservationOutcome};
        use std::time::Duration;

        let _home = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("queue-3621", "/tmp/aoe-3621-drain");
        inst.id = "sess-3621".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let service = service_for(vec![inst]);

        // Deliverable text, so the drain reaches `send_turn` rather than
        // retiring an undeliverable husk on the way.
        service
            .enqueue_prompt(
                "sess-3621",
                "q1".into(),
                "follow-up".into(),
                vec![],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");

        // Hold the reservation for the whole probe so no worker can land.
        let reservation = match service
            .acp_supervisor
            .begin_resume("sess-3621", ResumeKind::Spawn)
            .await
            .expect("begin_resume must not error under capacity")
        {
            ResumeReservationOutcome::Reserved(r) => r,
            ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
        };

        let mut waits = service.acp_supervisor.watch_worker_waits();
        let drain = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.drain_queued_prompts_once("sess-3621").await }
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), waits.recv())
                .await
                .expect("worker readiness reached")
                .expect("worker wait observation"),
            "sess-3621"
        );

        // The 2s budget is far under the 10s `WORKER_READY_TIMEOUT` the
        // pre-fix drain holds the lock for, and far over what the fixed one
        // needs, since it never takes the lock at all.
        let inst_lock = service.instance_lock("sess-3621").await;
        let acquired = tokio::time::timeout(Duration::from_secs(2), inst_lock.lock()).await;
        assert!(
            acquired.is_ok(),
            "the drain must leave instance_lock free for the resume's build_spawn_request"
        );
        drop(acquired);

        drop(reservation);
        tokio::time::timeout(Duration::from_secs(30), drain)
            .await
            .expect("the drain must finish once the reservation drops")
            .expect("drain task must not panic");

        assert_eq!(
            service.queued_prompts_snapshot("sess-3621").await.len(),
            1,
            "no worker ever arrived, so the batch stays queued for the next tick"
        );
    }

    /// Every queue mutation that can race a delivery waits for it.
    #[tokio::test]
    async fn queue_mutations_wait_for_an_in_flight_delivery() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        use std::time::Duration;

        let mut inst = Instance::new("queue-mut", "/tmp/aoe-queue-mutations");
        inst.id = "sess-mut".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let service = service_for(vec![inst]);
        service
            .enqueue_prompt(
                "sess-mut",
                "q1".into(),
                "original".into(),
                vec![],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");

        // Stand in for a drain holding the session across snapshot -> send.
        let delivering = service.prompt_submission("sess-mut").await;
        let mut claims = service.watch_submission_claims();
        let edit = {
            let service = Arc::clone(&service);
            async move {
                service
                    .edit_queued_prompt("sess-mut", "q1".into(), "edited".into())
                    .await
            }
        };
        tokio::pin!(edit);
        assert!(
            futures_util::poll!(&mut edit).is_pending(),
            "an edit must not rewrite a row a delivery has already snapshotted"
        );
        assert_eq!(
            claims
                .try_recv()
                .expect("contender reached submission claim"),
            "sess-mut"
        );
        drop(delivering);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(10), edit)
                .await
                .expect("the edit lands once the delivery releases the session"),
            EditQueuedOutcome::Updated
        ));

        let delivering = service.prompt_submission("sess-mut").await;
        assert_eq!(claims.try_recv().expect("holder claim"), "sess-mut");
        let clear = {
            let service = Arc::clone(&service);
            async move { service.clear_queued_prompts("sess-mut").await }
        };
        tokio::pin!(clear);
        assert!(
            futures_util::poll!(&mut clear).is_pending(),
            "a clear must not empty the queue out from under a delivery"
        );
        assert_eq!(
            claims
                .try_recv()
                .expect("contender reached submission claim"),
            "sess-mut"
        );
        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), clear)
            .await
            .expect("the clear lands once the delivery releases the session");
        assert!(service.queued_prompts_snapshot("sess-mut").await.is_empty());
    }

    /// #3687.
    #[tokio::test]
    async fn drains_leave_no_prompt_lock_for_a_deleted_session() {
        use std::time::Duration;

        fn drainable(id: &str) -> Instance {
            let mut inst = Instance::new("drain-del", "/tmp/aoe-3687");
            inst.id = id.to_string();
            inst.view = crate::session::View::Structured;
            inst.status = crate::session::Status::Idle;
            inst.pending_initial_turn = Some(crate::session::PendingInitialTurn {
                text: "hello".to_string(),
                attachments: Vec::new(),
                synthesized: false,
            });
            inst
        }

        let service = crate::server::test_support::build_test_app_state(vec![
            drainable("sess-3687-a"),
            drainable("sess-3687-b"),
            drainable("sess-3687-live"),
        ])
        .session_service
        .clone();

        // A surviving session's entry makes the assertions a return to a prior
        // size rather than an emptied map.
        drop(service.prompt_submission("sess-3687-live").await);
        let before = service.prompt_locks_len().await;
        assert_eq!(before, 1);

        // Window one: the delete completed before the reconciler's drain ran.
        service
            .instances
            .write()
            .await
            .retain(|i| i.id != "sess-3687-a");
        service.forget_prompt_lock("sess-3687-a").await;
        service.drain_pending_initial_turn("sess-3687-a").await;
        service.drain_queued_prompts_once("sess-3687-a").await;
        assert_eq!(
            service.prompt_locks_len().await,
            before,
            "a drain for an id that no longer exists must not vivify an entry"
        );

        // Window two.
        let mut claims = service.watch_submission_claims();
        let registry = service.prompt_locks.write().await;
        let drain = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.drain_pending_initial_turn("sess-3687-b").await }
        });
        loop {
            let claimed = tokio::time::timeout(Duration::from_secs(10), claims.recv())
                .await
                .expect("the drain must reach its submission claim")
                .expect("the tap outlives the drain");
            if claimed == "sess-3687-b" {
                break;
            }
        }
        service
            .instances
            .write()
            .await
            .retain(|i| i.id != "sess-3687-b");
        drop(registry);
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .expect("the drain finishes once the registry is free")
            .expect("drain task must not panic");
        assert_eq!(
            service.prompt_locks_len().await,
            before,
            "an entry vivified after the delete's removal must be retired by the drain"
        );
    }

    /// Queueing a follow-up is a user gesture, so it must advance `last_accessed_at` in
    /// memory and on disk, and it must do so WITHOUT clearing a sink a peer wrote after
    /// this daemon's snapshot.
    #[tokio::test]
    #[serial_test::serial]
    async fn enqueueing_a_prompt_advances_recency_without_clearing_a_peer_sink() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let profile = "default";

        let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
        let mut inst = Instance::new("queue", "/tmp/aoe-queue-recency");
        inst.id = "sess-recency".to_string();
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        inst.last_accessed_at = Some(stale);

        // Disk carries a sink this daemon's memory has not observed, which is
        // exactly the shape #3465's wipe needs.
        let mut seed = inst.clone();
        seed.archive();
        let peer_archived_at = seed.archived_at;
        assert!(peer_archived_at.is_some());
        seed.last_accessed_at = Some(stale);
        crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .update(|instances, _groups| {
                instances.push(seed);
                Ok(())
            })
            .unwrap();

        let service = service_for(vec![inst]);
        service
            .enqueue_prompt(
                "sess-recency",
                "q1".into(),
                "follow-up behind a live turn".into(),
                vec![],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");

        let in_memory = service.instances.read().await[0].last_accessed_at;
        assert!(
            in_memory > Some(stale),
            "queueing is a user gesture; daemon memory must advance recency"
        );

        let on_disk = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        let row = on_disk
            .iter()
            .find(|i| i.id == "sess-recency")
            .expect("session on disk");
        assert!(
            row.last_accessed_at > Some(stale),
            "the gesture must survive a daemon restart, not just live in memory"
        );
        assert_eq!(
            row.archived_at, peer_archived_at,
            "a queued prompt is a recency advance, not a wake: it must not clear \
             a peer archive (#3465)"
        );
    }

    /// Concurrent enqueues all survive with distinct seqs, in memory and on disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn concurrent_enqueues_all_survive_to_disk() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let profile = "default";

        let mut inst = Instance::new("queue", "/tmp/aoe-queue-concurrent");
        inst.id = "sess-cc".to_string();
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        let seed = inst.clone();
        crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .update(|instances, _groups| {
                instances.push(seed);
                Ok(())
            })
            .unwrap();
        let service = service_for(vec![inst]);

        // Fire them together, the way two quick taps on Queue do.
        let mut tasks = Vec::new();
        for i in 0..32 {
            let svc = Arc::clone(&service);
            tasks.push(tokio::spawn(async move {
                svc.enqueue_prompt(
                    "sess-cc",
                    format!("p{i}"),
                    format!("prompt {i}"),
                    vec![],
                    None,
                    "t".into(),
                )
                .await
            }));
        }
        for t in tasks {
            t.await.expect("task").expect("session exists");
        }

        let in_memory = service.queued_prompts_snapshot("sess-cc").await;
        assert_eq!(in_memory.len(), 32, "every enqueue lands in memory");
        // Seqs are unique, so the drain order is well defined.
        let mut seqs: Vec<u64> = in_memory.iter().map(|q| q.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 32, "no two rows share a seq");

        let on_disk = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        let persisted = &on_disk
            .iter()
            .find(|i| i.id == "sess-cc")
            .expect("session on disk")
            .queued_prompts;
        assert_eq!(persisted.len(), 32, "every enqueue reaches disk");
        // The disk-side check is the one that fails on finding 8.
        let mut disk_seqs: Vec<u64> = persisted.iter().map(|q| q.seq).collect();
        disk_seqs.sort_unstable();
        disk_seqs.dedup();
        assert_eq!(disk_seqs.len(), 32, "no two persisted rows share a seq");
    }

    /// A disk reload whose snapshot predates an in-memory row change must not
    /// replace that change with the stale disk row.
    #[tokio::test]
    async fn a_stale_disk_reload_keeps_in_memory_row_changes() {
        use crate::session::PendingInitialTurn;
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let pending = || PendingInitialTurn {
            text: "resume".into(),
            attachments: vec![],
            synthesized: true,
        };
        type Mutate = fn(Arc<SessionService>) -> futures_util::future::BoxFuture<'static, ()>;
        type Check = fn(&Instance) -> bool;
        let cases: [(&str, Option<PendingInitialTurn>, bool, Mutate, Check); 4] = [
            (
                "enqueue",
                None,
                false,
                |s| {
                    Box::pin(async move {
                        s.enqueue_prompt("s", "p".into(), "t".into(), vec![], None, "t0".into())
                            .await;
                    })
                },
                |i| i.queued_prompts.len() == 1,
            ),
            (
                "set pending turn",
                None,
                false,
                |s| {
                    Box::pin(
                        async move { s.set_pending_initial_turn("s", "r".into(), vec![]).await },
                    )
                },
                |i| i.pending_initial_turn.is_some(),
            ),
            (
                "clear pending turn",
                Some(pending()),
                false,
                |s| Box::pin(async move { s.clear_pending_initial_turn("s").await }),
                |i| i.pending_initial_turn.is_none(),
            ),
            (
                "prompt wakes a dormant session",
                None,
                true,
                |s| {
                    Box::pin(async move {
                        s.touch_and_wake_on_prompt("s", false).await;
                    })
                },
                |i| !i.is_idle_dormant(),
            ),
        ];
        for (name, pending_turn, dormant, mutate, check) in cases {
            let mut inst = Instance::new("race", "/tmp/aoe-reload-race");
            inst.id = "s".to_string();
            inst.view = crate::session::View::Structured;
            inst.pending_initial_turn = pending_turn;
            inst.idle_dormant_since = dormant.then(chrono::Utc::now);
            let state = crate::server::test_support::build_test_app_state(vec![inst.clone()]);
            let read_epoch = state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst);

            mutate(Arc::clone(&state.session_service)).await;
            crate::server::reload::reload_state_instances_from_disk(
                &state,
                vec![inst],
                vec![],
                crate::server::state::StatusSource::DiskOnly,
                read_epoch,
            )
            .await;

            assert!(check(&state.instances.read().await[0]), "{name}");
        }
    }

    #[tokio::test]
    async fn queue_store_enqueue_edit_remove_clear() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("queue", "/tmp/aoe-queue-project");
        inst.id = "sess-q".to_string();
        inst.view = crate::session::View::Structured;
        let service = service_for(vec![inst]);

        // Enqueue two.
        let a = service
            .enqueue_prompt(
                "sess-q",
                "a".into(),
                "first".into(),
                vec![],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");
        let b = service
            .enqueue_prompt(
                "sess-q",
                "b".into(),
                "second".into(),
                vec![],
                None,
                "t1".into(),
            )
            .await
            .expect("session exists");
        assert_eq!((a.seq, b.seq), (0, 1));
        let snap = service.queued_prompts_snapshot("sess-q").await;
        assert_eq!(
            snap.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );

        // Re-enqueue by the same id is an idempotent update, not a duplicate.
        let a2 = service
            .enqueue_prompt(
                "sess-q",
                "a".into(),
                "first edited".into(),
                vec![],
                None,
                "t2".into(),
            )
            .await
            .expect("session exists");
        assert_eq!(a2.seq, 0, "re-enqueue keeps the original seq");
        assert_eq!(service.queued_prompts_snapshot("sess-q").await.len(), 2);

        // Edit / remove / clear.
        assert_eq!(
            service
                .edit_queued_prompt("sess-q", "b".into(), "second edited".into())
                .await,
            EditQueuedOutcome::Updated
        );
        assert_eq!(
            service
                .edit_queued_prompt("sess-q", "missing".into(), "x".into())
                .await,
            EditQueuedOutcome::NotFound
        );
        // Blanking a text-only row is refused and leaves the text intact.
        for blank in ["", "   ", "\n\t "] {
            assert_eq!(
                service
                    .edit_queued_prompt("sess-q", "b".into(), blank.into())
                    .await,
                EditQueuedOutcome::WouldEmpty,
                "{blank:?}"
            );
        }
        assert_eq!(
            service
                .queued_prompts_snapshot("sess-q")
                .await
                .iter()
                .find(|q| q.id == "b")
                .map(|q| q.text.as_str()),
            Some("second edited"),
            "a refused edit must not have mutated the row"
        );
        assert!(service.remove_queued_prompt("sess-q", "a".into()).await);
        assert!(!service.remove_queued_prompt("sess-q", "a".into()).await);
        let snap = service.queued_prompts_snapshot("sess-q").await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].text, "second edited");
        service.clear_queued_prompts("sess-q").await;
        assert!(service.queued_prompts_snapshot("sess-q").await.is_empty());

        // A gone session is a None/no-op, never a panic.
        assert!(service
            .enqueue_prompt(
                "sess-gone",
                "z".into(),
                "x".into(),
                vec![],
                None,
                "t".into()
            )
            .await
            .is_none());
    }

    #[tokio::test]
    async fn wake_dormant_for_queue_drain_clears_only_when_dormant() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        // A session the idle reaper auto-stopped.
        let mut dormant = Instance::new("queue", "/tmp/aoe-queue-dormant");
        dormant.id = "sess-dormant".to_string();
        dormant.view = crate::session::View::Structured;
        dormant.mark_idle_dormant();
        assert!(dormant.is_idle_dormant());

        // A live/idle session that is not dormant must be left untouched.
        let mut awake = Instance::new("queue", "/tmp/aoe-queue-awake");
        awake.id = "sess-awake".to_string();
        awake.view = crate::session::View::Structured;

        let state = crate::server::test_support::build_test_app_state(vec![dormant, awake]);
        let service = state.session_service.clone();

        service.wake_dormant_for_queue_drain("sess-dormant").await;
        service.wake_dormant_for_queue_drain("sess-awake").await;
        // A gone session is a no-op, never a panic.
        service.wake_dormant_for_queue_drain("sess-gone").await;

        let instances = state.instances.read().await;
        let dormant_after = instances.iter().find(|i| i.id == "sess-dormant").unwrap();
        let awake_after = instances.iter().find(|i| i.id == "sess-awake").unwrap();
        assert!(
            !dormant_after.is_idle_dormant(),
            "dormant queued session must be woken so the resume pass respawns it"
        );
        assert!(
            !awake_after.is_idle_dormant(),
            "a non-dormant session must stay non-dormant (no spurious wake)"
        );
    }

    #[test]
    fn queue_drain_batch_splits_on_clear_boundary() {
        use crate::daemon::QueuedPromptEntry;
        let entry = |id: &str, seq: u64, text: &str| QueuedPromptEntry {
            id: id.into(),
            seq,
            text: text.into(),
            attachments: vec![],
            created_at: "t".into(),
            origin_device: None,
        };
        // claude's profile clears with "/clear".
        let claude = crate::acp::agent_profiles::resolve("claude");
        assert!(
            !claude.clear_aliases.is_empty(),
            "test assumes claude has a clear alias"
        );

        // A leading run of non-clear rows combines up to (not including) the
        // first clear command.
        let q = vec![
            entry("a", 0, "one"),
            entry("b", 1, "two"),
            entry("c", 2, "/clear"),
            entry("d", 3, "three"),
        ];
        let (sub, combined) = queue_drain_batch(&q, claude);
        assert_eq!(
            sub.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(combined, "one\n\ntwo");

        // A clear command at the head fires as its own turn.
        let q = vec![entry("c", 0, "/clear"), entry("a", 1, "one")];
        let (sub, combined) = queue_drain_batch(&q, claude);
        assert_eq!(sub.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["c"]);
        assert_eq!(combined, "/clear");

        // No clear anywhere.
        let q = vec![
            entry("a", 0, "one"),
            entry("b", 1, ""),
            entry("c", 2, "three"),
        ];
        let (sub, combined) = queue_drain_batch(&q, claude);
        assert_eq!(sub.len(), 3);
        assert_eq!(combined, "one\n\nthree");
    }
}

//! Shared session-domain service handle.
//!
//! Holds the narrow set of daemon state the session create/turn paths need
//! (live instances, ACP supervisor, storage file-watch, per-instance locks,
//! telemetry counter), so those paths can be driven by callers that do not
//! hold the HTTP `AppState`: today the HTTP handlers, next the plugin host
//! RPCs (#2897). `AppState` constructs one and keeps cloned handles to the
//! same underlying state, so both views stay consistent; neither owns the
//! other, which avoids an `AppState`/`PluginHost` reference cycle.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::server::session_spawn::{spawn_structured_session, SpawnOutcome, StructuredSessionSpec};
use crate::session::{Instance, PluginCreateIdempotency};

/// A create currently being built for a `(plugin_id, idempotency_key)` scope.
/// Present only between the idempotency claim and the end of the build, so a
/// concurrent retry of the same key waits for the winner instead of
/// provisioning a second worktree.
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

/// Marker error for a plugin create that reused an idempotency key with a
/// different request payload. Callers downcast it the same way the HTTP
/// handler downcasts `SessionBuildPanicked` / `HooksNeedTrust`.
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

/// The `Instance` fields `mutate_instance_persisted` copies from memory to
/// disk. Deliberately explicit: the disk write no longer re-runs the caller's
/// closure, so a field that is not listed here is simply not persisted.
#[cfg(feature = "serve")]
struct MirroredFields {
    queued_prompts: Vec<crate::acp::state::QueuedPromptEntry>,
    queued_prompt_next_seq: u64,
    idle_dormant_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Mirrored as `disk = max(disk, memory)`, not copied, because the field is
    /// documented monotone non-decreasing and disk can legitimately lead memory
    /// when a peer process touched the row.
    ///
    /// Note what that means for the callers that do not advance it themselves
    /// (edit / remove / clear / the dormancy clear): the max is NOT a no-op for
    /// them. It runs against the disk row, so whenever memory leads disk those
    /// mutations flush memory's value too. That is intended. Since #3465 and
    /// #3481 removed the passive stamps, a leading memory value can only have
    /// come from a real user gesture that has not reached disk yet, so
    /// persisting it opportunistically is correct rather than fabricated.
    ///
    /// It is also safe in the direction #3465 cares about: a max advances
    /// recency but never clears `archived_at` / `snoozed_until` /
    /// `idle_dormant_since`, so no queue mutation can lift a sink. Writing this
    /// as `touch_last_accessed()` would.
    last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of `SessionService::edit_queued_prompt`.
#[cfg(feature = "serve")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditQueuedOutcome {
    Updated,
    /// No row with that `prompt_id` in the session's queue.
    NotFound,
    /// The edit would leave a row with neither text nor attachments, which the
    /// drain cannot deliver. Refused at the door: the drain now retires such a
    /// row rather than wedging on it, but silently discarding what the user
    /// typed is a worse outcome than a 400.
    WouldEmpty,
}

/// Pick the leading drain batch from a session's queue and its combined text,
/// matching the client's `useAcpSession` split exactly: a clear-command row at
/// the head fires as its own turn; otherwise the leading run of non-clear rows
/// combines with blank-line separators (empty-text rows skipped). An agent with
/// no clear aliases combines the whole queue. Pure, so the boundary logic is
/// unit-tested without a live worker.
#[cfg(feature = "serve")]
fn queue_drain_batch<'a>(
    queue: &'a [crate::acp::state::QueuedPromptEntry],
    profile: &crate::acp::agent_profiles::AgentProfile,
) -> (&'a [crate::acp::state::QueuedPromptEntry], String) {
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
    /// Opt-in telemetry create counter, shared with
    /// `AppState.telemetry_session_creates`.
    pub telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
    /// Owns the per-session ACP agent subprocesses, shared with
    /// `AppState.acp_supervisor`.
    #[cfg(feature = "serve")]
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    /// Durable ACP event store, shared with `AppState.acp_event_store`. Used
    /// by the pending-turn drain to reload attachment blobs for a rate-limit
    /// resume continuation (#3028).
    #[cfg(feature = "serve")]
    pub acp_event_store: Arc<crate::acp::event_store::EventStore>,
    /// In-flight plugin creates keyed by `(plugin_id, idempotency_key)`.
    /// Sync mutex: critical sections are tiny and never span an `await`.
    // ponytail: one daemon process is the only sessions.json writer, so a
    // process-local registry closes the duplicate-create race; a cross-process
    // reservation store only becomes necessary if that assumption changes.
    create_in_flight: std::sync::Mutex<HashMap<(String, String), CreateInFlight>>,
    /// Session ids with a pending-initial-turn drain in flight, so the create
    /// fast path and the reconciler tick cannot queue duplicate drains.
    /// Sync mutex: critical sections are tiny and never span an `await`.
    pending_drains: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Per-session persist locks for `mutate_instance_persisted`, held across
    /// snapshot AND disk write so the two cannot be reordered. Distinct from
    /// `instance_locks`: the drain holds that one across `send_turn` and then
    /// calls this path, so reusing it would self-deadlock.
    persist_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Who is asking the session service to act. Constructed only by the
/// transport layer (HTTP handler, plugin RPC connection context, or the
/// drain reconstructing the creator), never decoded from a request payload,
/// so a caller cannot forge an identity (#2897).
#[cfg(feature = "serve")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCaller {
    /// A human-facing surface (HTTP dashboard, TUI).
    User,
    /// A plugin worker, identified by its connection's plugin id.
    Plugin { plugin_id: String },
}

/// Typed outcome of [`SessionService::send_turn`], split by whether the
/// failure happened before or after the prompt was published into the event
/// stream, so callers can map each stage faithfully (the HTTP handler keeps
/// its exact pre-extraction status codes, and only fires the post-publish
/// smart-rename hook when a publish actually happened).
#[cfg(feature = "serve")]
pub(crate) enum SendTurnError {
    /// Pre-publish: the session vanished (or was triaged) before the resume
    /// snapshot. Nothing was published; the honest answer is "not found",
    /// not a retryable worker_not_ready. See #1748.
    SessionNotFound,
    /// Pre-publish: a plugin caller targeted a session it did not create
    /// (user-created, another plugin's, or a legacy row). Nothing was
    /// published; no side effects ran.
    NotOwner,
    /// Pre-publish: the session's persisted explicit mode could not be
    /// re-asserted before a plugin-delivered turn. The prompt is withheld
    /// rather than run under an unconfirmed approval posture.
    ModeApplication(crate::acp::supervisor::SupervisorError),
    /// Pre-publish: reserving the resume slot failed (includes
    /// `SupervisorError::CapacityFull`). Nothing was published.
    ResumeFailed(crate::acp::supervisor::SupervisorError),
    /// Pre-publish: the worker did not become ready within
    /// `WORKER_READY_TIMEOUT` (slow sandbox / spawn). The worker is still
    /// coming; retryable, and nothing was published, so the transcript is
    /// not left with a prompt no agent ever received. See #1748 and #3172.
    WorkerNotReady,
    /// Post-publish: the forward to the agent failed.
    Send(crate::acp::supervisor::SupervisorError),
}

#[cfg(feature = "serve")]
impl std::fmt::Display for SendTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => write!(f, "session not found"),
            Self::NotOwner => write!(f, "session was not created by the calling plugin"),
            Self::ModeApplication(e) => write!(f, "mode application failed: {e}"),
            Self::ResumeFailed(e) => write!(f, "worker resume failed: {e}"),
            Self::WorkerNotReady => write!(f, "worker not ready"),
            Self::Send(e) => write!(f, "prompt forward failed: {e}"),
        }
    }
}

impl SessionService {
    #[cfg(feature = "serve")]
    pub fn new(
        instances: Arc<RwLock<Vec<Instance>>>,
        instance_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
        file_watch: Arc<crate::file_watch::FileWatchService>,
        telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
        acp_supervisor: Arc<
            crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
        >,
        acp_event_store: Arc<crate::acp::event_store::EventStore>,
    ) -> Self {
        Self {
            instances,
            instance_locks,
            file_watch,
            telemetry_session_creates,
            acp_supervisor,
            acp_event_store,
            create_in_flight: std::sync::Mutex::new(HashMap::new()),
            pending_drains: std::sync::Mutex::new(std::collections::HashSet::new()),
            persist_locks: RwLock::new(HashMap::new()),
        }
    }

    #[cfg(not(feature = "serve"))]
    pub fn new(
        instances: Arc<RwLock<Vec<Instance>>>,
        instance_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
        file_watch: Arc<crate::file_watch::FileWatchService>,
        telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        Self {
            instances,
            instance_locks,
            file_watch,
            telemetry_session_creates,
            create_in_flight: std::sync::Mutex::new(HashMap::new()),
            pending_drains: std::sync::Mutex::new(std::collections::HashSet::new()),
            persist_locks: RwLock::new(HashMap::new()),
        }
    }

    /// Create a structured session through the shared spawn pipeline,
    /// optionally as a plugin with a create-idempotency key (#2897).
    ///
    /// For a user caller (`plugin_id: None`) this is exactly the pre-service
    /// create path. For a plugin caller it additionally:
    /// - stamps `created_by_plugin` and the idempotency record on the
    ///   instance before it is persisted, atomically with the row itself;
    /// - forces repo-hook trust fail-closed (`trust_hooks = false`); a plugin
    ///   cannot pre-approve a repository's hooks, so an untrusted repo
    ///   refuses the create regardless of install grants;
    /// - deduplicates on `(plugin_id, idempotency_key)`: a retry with the
    ///   same payload returns the existing session (`created: false` in the
    ///   returned pair), a retry with a different payload fails with
    ///   [`IdempotencyConflict`], and a concurrent identical retry waits for
    ///   the in-flight build instead of provisioning a second worktree.
    ///
    /// Idempotency retention equals the session record's lifetime: archived,
    /// snoozed, and trashed sessions still deduplicate; a hard-deleted
    /// session releases its key, and a later retry creates a fresh session.
    ///
    /// Returns the spawn outcome plus `created`: `false` when an existing
    /// session was returned by idempotency instead of a new one.
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
            // Persisted-first lookup: a completed create (this daemon life or
            // an earlier one) wins before any in-flight coordination.
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
                    // The winner removes its entry and notifies on every exit
                    // path (guard drop), after which the loop re-checks the
                    // persisted list: a successful winner is found there, a
                    // failed winner leaves this retry to build fresh. The
                    // wait is bounded because `notify_waiters` only wakes
                    // already-registered waiters; a winner finishing between
                    // our claim attempt and this await would otherwise strand
                    // us. A missed notify costs one extra loop iteration.
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

    /// Resolve a persisted plugin create-idempotency decision without any side
    /// effect, so a caller can charge admission (rate/concurrency) only for
    /// genuinely new creates (#2897). `spec` must be the exact spec that will
    /// be passed to [`Self::create_structured_session`] (in particular
    /// `pending_initial_turn` already set), so the payload hash matches. Only
    /// the persisted list is consulted: an in-flight same-process retry still
    /// dedupes inside `create_structured_session`, at the cost of one admission.
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

    /// Deliver a turn to a structured session: resume a dead/dormant worker
    /// if needed, publish the prompt into the event stream, then forward it
    /// to the agent. Extracted from the `acp_prompt` HTTP handler so a
    /// non-HTTP caller (the plugin host, #2897) delivers turns through the
    /// same path; the handler keeps HTTP concerns (read-only gate, wake,
    /// attachment validation, smart-rename, status mapping).
    ///
    /// `woke_idle_dormant` forces the resume trigger even when the worker
    /// looks alive, mirroring the handler's idle-dormant wake (#1689).
    #[cfg(feature = "serve")]
    pub(crate) async fn send_turn(
        self: &Arc<Self>,
        caller: &SessionCaller,
        id: &str,
        text: &str,
        attachments: &[crate::acp::event_store::AttachmentBlob],
        woke_idle_dormant: bool,
        prompt_id: Option<String>,
    ) -> Result<(), SendTurnError> {
        use crate::server::acp_reconciler::ResumeTrigger;
        // Ownership gate, before ANY side effect (no wake, resume, publish,
        // or forward for a denied caller): a plugin may deliver turns only
        // to sessions it created. Ownership is immutable after creation, so
        // a read snapshot suffices; deliberately no instance_lock here (the
        // pending-turn drain calls this while holding it).
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
        // Resume a worker that is not currently live. Two cases:
        //   - Idle-dormant wake: the worker was auto-stopped for inactivity
        //     (#1689) and the reconciler will not respawn it until its next
        //     ~2s tick.
        //   - Dead worker: the worker exited for another reason (e.g. the
        //     silent-orphan watchdog escalated a monitor / `/loop` turn) and
        //     is neither dormant nor mid-respawn, so a send would otherwise
        //     404 and force a manual `aoe acp restart`.
        // Either way, reserve the resume slot synchronously and drive a fresh
        // spawn in a detached task NOW so the `send_prompt` below blocks on
        // `wait_for_worker` until the worker is live instead of racing ahead
        // to a 404. The detached task survives the originating request being
        // cancelled on client disconnect. `is_running` is true for a live or
        // mid-respawn worker, so a healthy session never double-spawns. See
        // #1748.
        let needs_resume = woke_idle_dormant || !self.acp_supervisor.is_running(id).await;
        if needs_resume {
            match crate::server::acp_reconciler::trigger_resume_background(self, id).await {
                Ok(ResumeTrigger::NotFound) => return Err(SendTurnError::SessionNotFound),
                Ok(_) => {}
                Err(e) => return Err(SendTurnError::ResumeFailed(e)),
            }
        }
        // Gate the publish below on the worker actually being there. Without
        // this the prompt lands in the durable event stream and only THEN
        // does `send_prompt` discover the respawn never finished, leaving a
        // `UserPromptSent` with no turn behind it and a UI stuck on
        // "running" until someone stops the session and re-sends (#3172).
        // Failing here instead returns a 503 the frontend already treats as
        // transient: it rolls the optimistic row back and re-queues for the
        // drain to re-fire on the next `AcpSessionAssigned` (#3094).
        //
        // Unconditional, not gated on `needs_resume`: `is_running` is also
        // true for a resume another caller already reserved, so a false
        // `needs_resume` does not imply a live worker. For one that is live
        // this is a single worker-map lookup.
        if let Err(e) = self.acp_supervisor.wait_until_ready(id).await {
            return match e {
                crate::acp::supervisor::SupervisorError::UnknownSession(_) => {
                    Err(SendTurnError::WorkerNotReady)
                }
                other => Err(SendTurnError::ResumeFailed(other)),
            };
        }
        // A plugin-delivered turn must run under the session's persisted
        // explicit mode: re-assert it before publishing, and withhold the
        // prompt when the assertion fails (#2897). set_mode waits on the
        // same ready-client path send_prompt uses, so a just-resumed worker
        // is awaited, not raced. User surfaces skip this; the supervisor
        // already re-asserts the mode on every (re)spawn.
        if matches!(caller, SessionCaller::Plugin { .. }) {
            if let Some(mode_id) = &acp_mode_id {
                if let Err(e) = self.acp_supervisor.set_mode(id, mode_id).await {
                    return Err(SendTurnError::ModeApplication(e));
                }
            }
        }
        // Publish the user's prompt into the event stream BEFORE forwarding
        // to the agent so the replay buffer / on-disk store captures it
        // even if the agent forward fails. The frontend treats UserPromptSent
        // as authoritative and dedupes against its own optimistic row.
        //
        // The publish step owns clear-command detection and tells us what to
        // do with the text: either forward it as an ordinary prompt or drive
        // a real reset on the live worker for a clear alias whose adapter
        // cannot hand back a durable post-reset session id. Forwarding a codex
        // `/new` would be swallowed as an unknown command and the conversation
        // would silently keep its context (#2979); forwarding a claude
        // `/clear` resets the context but leaves the new conversation
        // unresumable across a worker restart (upstream #906).
        let disposition = self
            .acp_supervisor
            .publish_user_prompt_with_attachments(id, text.to_string(), attachments, prompt_id)
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
            // Intentional override of the canonical UnknownSession 404: the
            // readiness barrier above passed, so the worker was alive a
            // moment ago and died in the gap. Retryable rather than a 404,
            // same as before. This one IS post-publish; closing that window
            // needs delivery acknowledgements, not a longer timeout.
            Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) if needs_resume => {
                Err(SendTurnError::WorkerNotReady)
            }
            Err(e) => Err(SendTurnError::Send(e)),
        }
    }

    /// Deliver a session's persisted `pending_initial_turn`, then clear it.
    ///
    /// Single drain owner: callers (the create fast path and the reconciler
    /// tick) race through the `pending_drains` claim, and the delivery runs
    /// under the per-instance lock, so the turn cannot be published twice
    /// concurrently. A delivery failure leaves the field set; the reconciler
    /// tick retries once the worker is live. Clearing writes memory first,
    /// then disk: a crash (or failed persist) between the forward and the
    /// disk clear re-delivers after restart, which is the documented
    /// at-least-once contract.
    #[cfg(feature = "serve")]
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
        let inst_lock = self.instance_lock(id).await;
        let _serialized = inst_lock.lock().await;
        let Some((text, attachment_refs, profile, caller)) = ({
            let instances = self.instances.read().await;
            instances.iter().find(|i| i.id == id).and_then(|i| {
                i.pending_initial_turn.clone().map(|text| {
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
                        text,
                        i.pending_initial_turn_attachments.clone(),
                        i.source_profile.clone(),
                        caller,
                    )
                })
            })
        }) else {
            return;
        };
        // Reload the attachment blobs so a rate-limit resume continuation
        // replays the interrupted prompt's images/files, not just its text
        // (#3028). Refs are empty for create-time initial turns. Bytes live in
        // the event store (a locking sqlite read), so load off the runtime.
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
            .send_turn(&caller, id, &text, &attachments, false, None)
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
                inst.pending_initial_turn_attachments = Vec::new();
            }
        }
        match crate::session::Storage::new(&profile, self.file_watch.clone()) {
            Ok(storage) => {
                let id_persist = id.to_string();
                let persisted = tokio::task::spawn_blocking(move || {
                    storage.update(|instances, _groups| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                            inst.pending_initial_turn = None;
                            inst.pending_initial_turn_attachments = Vec::new();
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

    /// Queue `text` (with its `attachments` refs) as the session's next turn,
    /// reusing the pending-initial-turn drain so the turn is delivered once the
    /// (resumed) worker is live. No-op when a turn is already queued (never
    /// clobber a create/plugin turn) or the session is gone. Persists so a
    /// daemon restart mid-resume still re-delivers. Used to continue a
    /// rate-limit-interrupted turn on resume (#3028).
    #[cfg(feature = "serve")]
    pub(crate) async fn set_pending_initial_turn(
        self: &Arc<Self>,
        id: &str,
        text: String,
        attachments: Vec<crate::acp::state::PromptAttachmentRef>,
    ) {
        let profile = {
            let mut instances = self.instances.write().await;
            match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) if inst.pending_initial_turn.is_none() => {
                    inst.pending_initial_turn = Some(text.clone());
                    inst.pending_initial_turn_attachments = attachments.clone();
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
                            inst.pending_initial_turn = Some(text);
                            inst.pending_initial_turn_attachments = attachments;
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

    /// Drop any queued pending initial turn (text + attachment refs) for a
    /// session, in memory and on disk. A newer user prompt supersedes a queued
    /// rate-limit resume continuation, so the stale continuation must not
    /// replay after the newer message (#3028). No-op when nothing is queued.
    #[cfg(feature = "serve")]
    pub(crate) async fn clear_pending_initial_turn(self: &Arc<Self>, id: &str) {
        let profile = {
            let mut instances = self.instances.write().await;
            match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) if inst.pending_initial_turn.is_some() => {
                    inst.pending_initial_turn = None;
                    inst.pending_initial_turn_attachments = Vec::new();
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
                            inst.pending_initial_turn_attachments = Vec::new();
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

    /// Apply `mutate` to a session's in-memory `Instance`, then mirror the
    /// resulting state to disk. Returns what `mutate` produced, or `None` if
    /// the session is gone.
    ///
    /// `mutate` runs exactly once, against the in-memory instance, which is the
    /// authoritative copy: every queue mutation takes `instances.write()`, so
    /// that list is always correctly ordered. The disk write then *copies* the
    /// post-mutation fields rather than re-running the closure.
    ///
    /// Re-running it was wrong under concurrency. The instances lock is
    /// released before the disk write, so two concurrent enqueues could reach
    /// `storage.update` in either order and each re-derive `seq` from whatever
    /// the on-disk copy happened to hold, persisting an order the in-memory
    /// list never had (or losing a row entirely). Copying instead means the
    /// last writer persists the complete, correct state.
    ///
    /// Only the fields listed in `MirroredFields` survive to disk, which covers
    /// every caller today (the five queue mutations plus the dormancy clear).
    /// A new caller that mutates something else must extend that struct, so the
    /// set is explicit rather than implied by whatever the closure touched.
    ///
    /// Snapshot and disk write happen under one per-session persist lock.
    /// `Storage::update` serializes the writes themselves but not their order,
    /// so without this two concurrent mutations could snapshot as `[a]` then
    /// `[a, b]` and land in the opposite order, leaving `b` off disk and losing
    /// it on the next daemon restart. Copying a whole snapshot makes ordering
    /// load-bearing in a way that re-running the closure did not, so the lock
    /// comes with it.
    #[cfg(feature = "serve")]
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
                            // Monotone max, never `touch_last_accessed()`: a
                            // queued prompt is a recency gesture, not a wake,
                            // so it must not clear `archived_at` /
                            // `snoozed_until` / `idle_dormant_since` a peer
                            // wrote after this daemon's snapshot (#3465).
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

    /// Append a prompt to the session's server-owned queue and return the
    /// stored entry (with its assigned `seq`). Idempotent on `prompt_id`:
    /// re-enqueuing an existing id updates its text/attachments in place instead
    /// of adding a duplicate, so an optimistic client retry cannot double-queue.
    ///
    /// Queueing is a user gesture, so it advances `last_accessed_at`. Both
    /// enqueue routes land here (`POST /queue` from the web composer, and
    /// `/acp/prompt`'s Tier 3 `Queued` disposition, which the TUI takes because
    /// it always posts the prompt endpoint), so the two clients stay symmetric
    /// without either endpoint plumbing recency itself. It is only a recency
    /// advance, never a wake: the respawn kick a real wake needs lives on the
    /// prompt endpoint, and a client queues behind a live turn, so there is no
    /// sunk row to lift here.
    ///
    /// Until #3465 this was invisible. `apply_status_intent` restamped the
    /// field on every worker-event transition, so a queued turn's Running/Idle
    /// edges kept recency fresh by accident; dropping that stamp is what made
    /// the missing gesture-side write observable in the attention sort and the
    /// TUI activity column.
    #[cfg(feature = "serve")]
    pub(crate) async fn enqueue_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
        text: String,
        attachments: Vec<crate::acp::state::PromptAttachmentRef>,
        origin_device: Option<String>,
        created_at: String,
    ) -> Option<crate::acp::state::QueuedPromptEntry> {
        self.mutate_instance_persisted(id, move |inst| {
            inst.last_accessed_at = inst.last_accessed_at.max(Some(chrono::Utc::now()));
            if let Some(existing) = inst.queued_prompts.iter_mut().find(|q| q.id == prompt_id) {
                existing.text = text.clone();
                existing.attachments = attachments.clone();
                return existing.clone();
            }
            let seq = inst.queued_prompt_next_seq;
            inst.queued_prompt_next_seq = seq.saturating_add(1);
            let entry = crate::acp::state::QueuedPromptEntry {
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
    ///
    /// Enforces the same non-empty invariant `queue_enqueue` does, because the
    /// drain relies on it: a row whose text is blank and which carries no
    /// attachments makes `drain_queued_prompts_once` return without retiring
    /// the batch, so it retries on every reconciler tick, nothing behind it
    /// ever drains, and the idle reaper (which skips a session with a queue)
    /// keeps its worker alive forever.
    #[cfg(feature = "serve")]
    pub(crate) async fn edit_queued_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
        text: String,
    ) -> EditQueuedOutcome {
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

    /// Remove a queued prompt by id. Returns `true` if a row was removed. Also
    /// drops any attachment bytes buffered for that prompt so they don't leak.
    ///
    /// Takes the per-instance lock the drain holds across its whole
    /// snapshot -> send -> retire, so a removal cannot land in the middle of a
    /// delivery. Without it the web's "Send now" (which removes the row, then
    /// POSTs the same text itself) could tap inside the drain window: the drain
    /// already holds its own copy of the batch, so both would deliver and the
    /// agent would see the prompt twice. Serialized, the removal either beats
    /// the drain's snapshot (the drain never sees the row) or follows its
    /// retire (the removal reports `false`, and the caller sends nothing).
    #[cfg(feature = "serve")]
    pub(crate) async fn remove_queued_prompt(
        self: &Arc<Self>,
        id: &str,
        prompt_id: String,
    ) -> bool {
        let inst_lock = self.instance_lock(id).await;
        let _serialized = inst_lock.lock().await;
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

    /// Drop every queued prompt for a session, plus every attachment blob
    /// buffered for those prompts.
    #[cfg(feature = "serve")]
    pub(crate) async fn clear_queued_prompts(self: &Arc<Self>, id: &str) {
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
    #[cfg(feature = "serve")]
    pub(crate) async fn queued_prompts_snapshot(
        &self,
        id: &str,
    ) -> Vec<crate::acp::state::QueuedPromptEntry> {
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

    /// Drain the leading batch of a session's server-owned queue into the live
    /// worker once the current turn has ended. Mirrors
    /// `drain_pending_initial_turn`'s single-owner `pending_drains` claim +
    /// per-instance-lock delivery, so a batch is never sent twice concurrently.
    ///
    /// Only drains an idle turn: `Status::Idle` means the prior turn emitted its
    /// terminal `Stopped`, so this starts a fresh turn rather than racing a live
    /// one. Callers (the reconciler tick) gate on `is_running`, so the worker is
    /// live and `send_turn` will not spawn, making it safe to hold the instance
    /// lock across delivery (the #3172 re-entrant-spawn deadlock cannot occur on
    /// a live worker). The `/clear`-boundary split matches the client
    /// (`useAcpSession`): a clear-command row fires as its own turn; a leading
    /// run of non-clear rows combines into one with blank-line separators.
    /// A batch's buffered attachment bytes are reloaded and forwarded with it.
    #[cfg(feature = "serve")]
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
        let inst_lock = self.instance_lock(id).await;
        let _serialized = inst_lock.lock().await;

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

        // Leading batch up to a clear boundary (mirrors the client's split).
        let profile = crate::acp::agent_profiles::resolve(&agent_key);
        let (sub, combined) = queue_drain_batch(&queue, profile);
        let sent_ids: Vec<String> = sub.iter().map(|e| e.id.clone()).collect();
        // Reload every buffered attachment blob for the batch, in queue order,
        // so a queued screenshot/file is forwarded with the text (matches the
        // client's old `snapshot.flatMap(q => q.attachments)`). Bytes live in
        // the pending-attachment store keyed by prompt id; a locking SQLite
        // read, so do it off the runtime.
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
        // An attachment-only batch has empty combined text but real blobs. A
        // batch with neither cannot be delivered at all, and must be retired
        // rather than left queued.
        //
        // This guard used to `return`, which wedged the session: the batch
        // stayed at the head of the queue, the drain retried it on every
        // reconciler tick, nothing behind it ever drained, and
        // `reap_idle_workers` (which skips a session holding a queue) kept the
        // agent subprocess alive forever. Two routes reach the state, so
        // guarding the entry points is not enough on its own: an attachment-only
        // prompt whose buffered bytes the 24h `PENDING_ATTACHMENT_TTL` sweep
        // reclaimed, and (before it was rejected) an edit that blanked a
        // text-only row. There is nothing left to send by this point, so the
        // rows are husks; log loudly and clear them.
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

        // Deliver as a fresh turn on the live worker. `send_turn` re-records the
        // blobs under the real `UserPromptSent` seq. On failure leave the rows
        // (and their buffered blobs) queued; the next tick retries.
        if let Err(e) = self
            .send_turn(&caller, id, &combined, &attachments, false, None)
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
    /// Shared by the delivered path and the undeliverable-husk path so both
    /// leave the queue and the pending-attachment store consistent.
    #[cfg(feature = "serve")]
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

    /// Clear the idle-dormant marker for a session that has queued work so the
    /// reconciler's resume pass respawns its worker, after which the next tick
    /// drains the queue (see `acp_reconciler::drain_queued_prompts`).
    ///
    /// Deliberately does NOT kick a resume directly: routing the wake through
    /// the resume pass keeps that pass's respawn budget/park guard and never
    /// spawns while holding a lock, so it cannot hit the #3172 re-entrant-spawn
    /// deadlock the way a `send_turn`-under-lock wake would. Persists the clear
    /// via the instances-write path (not `instance_lock`), so a daemon restart
    /// keeps the session awake and the write never blocks a spawn. Guards on
    /// `is_idle_dormant` so a session woken by another path between the
    /// reconciler snapshot and this call is left untouched.
    #[cfg(feature = "serve")]
    pub(crate) async fn wake_dormant_for_queue_drain(self: &Arc<Self>, id: &str) {
        self.mutate_instance_persisted(id, |inst| {
            if inst.is_idle_dormant() {
                inst.idle_dormant_since = None;
            }
        })
        .await;
    }

    /// Same lazy per-instance mutex registry as `AppState::instance_lock`;
    /// both operate on the shared map, so a lock taken through either handle
    /// excludes the other.
    /// Per-session lock ordering the snapshot-and-persist in
    /// `mutate_instance_persisted`. Always acquired BEFORE `instances.write()`
    /// and never while holding it, and nothing acquires `instance_lock` while
    /// holding this, so it cannot form a cycle with either.
    #[cfg(feature = "serve")]
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
}

/// Releases a session's `pending_drains` claim on every exit path of
/// [`SessionService::drain_pending_initial_turn`], including panics.
#[cfg(feature = "serve")]
struct PendingDrainGuard {
    service: Arc<SessionService>,
    id: String,
}

#[cfg(feature = "serve")]
impl Drop for PendingDrainGuard {
    fn drop(&mut self) {
        self.service
            .pending_drains
            .lock()
            .expect("pending_drains mutex poisoned")
            .remove(&self.id);
    }
}

/// Releases the in-flight slot and wakes waiters on every exit path of the
/// winning create, including an error return or a panic unwinding through
/// the caller.
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

/// Match a plugin create request against the persisted sessions by
/// `(created_by_plugin, idempotency key)`. Archived, snoozed, and trashed
/// sessions still match: the record exists, so the create already happened;
/// returning it does not restore or unarchive anything. Only a hard-deleted
/// record (absent from the list) frees the key.
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

/// Versioned, restart-stable hash of the semantic create request. Field order
/// is fixed and every field is length-prefixed by its `Debug`/value rendering
/// with a separator, so two different requests cannot collide by
/// concatenation. `trust_hooks` is excluded: it is forced for plugin callers
/// and never part of the request identity.
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
    #[cfg(feature = "serve")]
    {
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
    }
    use std::fmt::Write;
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            #[cfg(feature = "serve")]
            view: crate::session::View::Structured,
            #[cfg(feature = "serve")]
            agent_name: Some("claude".to_string()),
            #[cfg(feature = "serve")]
            agent_model: None,
            #[cfg(feature = "serve")]
            agent_effort: None,
            #[cfg(feature = "serve")]
            import_acp_session_id: None,
            #[cfg(feature = "serve")]
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

        // Adjacent-field concatenation must not collide: moving a suffix of
        // one field into the prefix of the next is a different request.
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

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn in_flight_claim_waits_same_hash_and_conflicts_on_mismatch() {
        let service = crate::server::test_support::build_test_app_state(Vec::new())
            .session_service
            .clone();
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

        let notified = tokio::spawn(async move { notify.notified().await });
        // Let the waiter task register on the notify before the guard fires
        // notify_waiters (deterministic on the current-thread test runtime).
        tokio::task::yield_now().await;
        drop(InFlightGuard {
            service: Arc::clone(&service),
            scope: scope.clone(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("guard drop must wake waiters")
            .expect("waiter task");

        let ClaimOutcome::Claimed = service.try_claim_in_flight(&scope, "hash-a") else {
            panic!("released scope must be claimable again");
        };
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn probe_resolves_replay_conflict_and_new() {
        // Seed a prior create whose stored hash matches `test_spec()`; the probe
        // must resolve replay/conflict from the persisted list alone, so a
        // caller can skip admission (rate/concurrency) for an idempotent retry.
        let spec = test_spec();
        let hash = spec_payload_hash(&spec);
        let mut prior = plugin_instance("cron", "job-1", &hash);
        prior.id = "sess-prior".to_string();
        let service = crate::server::test_support::build_test_app_state(vec![prior])
            .session_service
            .clone();

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

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn send_turn_enforces_plugin_ownership_before_any_side_effect() {
        let mut user_session = Instance::new("user-owned", "/tmp/aoe-2897-project");
        user_session.id = "sess-user".to_string();
        let mut cron_session = Instance::new("cron-owned", "/tmp/aoe-2897-project");
        cron_session.id = "sess-cron".to_string();
        cron_session.created_by_plugin = Some("cron".to_string());
        let service =
            crate::server::test_support::build_test_app_state(vec![user_session, cron_session])
                .session_service
                .clone();

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
                .send_turn(&cron, "sess-user", "hi", &[], false, None)
                .await,
            Err(SendTurnError::NotOwner)
        ));
        assert!(matches!(
            service
                .send_turn(&other, "sess-cron", "hi", &[], false, None)
                .await,
            Err(SendTurnError::NotOwner)
        ));
        assert!(matches!(
            service
                .send_turn(&cron, "sess-gone", "hi", &[], false, None)
                .await,
            Err(SendTurnError::SessionNotFound)
        ));

        // The owner passes the gate; these terminal-view test sessions fail
        // at a LATER stage (resume snapshot or worker capacity, both
        // environment dependent), proving the denials above came from the
        // ownership check specifically.
        assert!(!matches!(
            service
                .send_turn(&cron, "sess-cron", "hi", &[], false, None)
                .await,
            Ok(()) | Err(SendTurnError::NotOwner)
        ));
        assert!(!matches!(
            service
                .send_turn(&SessionCaller::User, "sess-user", "hi", &[], false, None)
                .await,
            Ok(()) | Err(SendTurnError::NotOwner)
        ));
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn drain_is_a_noop_without_a_pending_turn_and_releases_its_claim() {
        let mut inst = Instance::new("no-pending", "/tmp/aoe-2897-project");
        inst.id = "sess-drain".to_string();
        inst.view = crate::session::View::Structured;
        let service = crate::server::test_support::build_test_app_state(vec![inst])
            .session_service
            .clone();

        // No pending turn: returns without touching the supervisor. Missing
        // session: same. Both must release the per-session claim so a later
        // drain can run (the second call would return early if the first
        // leaked its claim, which this test cannot distinguish from a no-op,
        // so assert on the claim set directly).
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
    /// `PENDING_ATTACHMENT_TTL` sweep reclaims them; the row is not swept with
    /// them) has neither text nor blobs, so the drain can never deliver it.
    ///
    /// It must be retired anyway. Leaving it queued wedged the session: the
    /// drain retried the same head-of-queue batch every reconciler tick,
    /// nothing behind it drained, and `reap_idle_workers` skips a session
    /// holding a queue, so the agent subprocess was never reaped.
    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn an_undeliverable_queue_row_is_retired_instead_of_wedging_the_queue() {
        let mut inst = Instance::new("queue", "/tmp/aoe-queue-husk");
        inst.id = "sess-husk".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let service = crate::server::test_support::build_test_app_state(vec![inst])
            .session_service
            .clone();

        // An attachment-only prompt: empty text, refs but no buffered bytes,
        // which is exactly the post-sweep state.
        service
            .enqueue_prompt(
                "sess-husk",
                "husk".into(),
                String::new(),
                vec![crate::acp::state::PromptAttachmentRef {
                    id: "att-1".into(),
                    kind: crate::acp::state::PromptAttachmentKind::Image,
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

        // The husk has to be the whole batch to wedge: a deliverable row in the
        // same batch supplies the text, and the batch then sends and retires
        // normally. Alone (or alone ahead of a `/clear` boundary) it is the
        // head the drain retries forever.
        service.drain_queued_prompts_once("sess-husk").await;
        assert!(
            service
                .queued_prompts_snapshot("sess-husk")
                .await
                .is_empty(),
            "the husk is retired rather than retried forever"
        );

        // And the queue is genuinely usable again, not just empty: a fresh
        // prompt queues and is not blocked behind a ghost.
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

    /// Queueing a follow-up is a user gesture, so it must advance
    /// `last_accessed_at` in memory and on disk, and it must do so WITHOUT
    /// clearing a sink a peer wrote after this daemon's snapshot.
    ///
    /// Before #3465 the recency half was supplied by accident:
    /// `apply_status_intent` restamped the field on every worker-event
    /// transition, so the queued turn's Running/Idle edges kept it fresh.
    /// Dropping that stamp left the web composer's `POST /queue` path (the one
    /// it takes for a follow-up typed behind a live turn) advancing nothing, so
    /// an actively-queued session aged into the top of the attention sort as
    /// the most neglected one (`groups.rs::attention_session_key` sorts
    /// `last_accessed_at` ASC) and the TUI activity column showed a stale age.
    ///
    /// The sink assertion is the other half: mirroring this field with
    /// `touch_last_accessed()` instead of a monotone max would pass the recency
    /// checks and reintroduce #3465's wipe.
    #[cfg(feature = "serve")]
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

        let service = crate::server::test_support::build_test_app_state(vec![inst])
            .session_service
            .clone();
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

    /// Concurrent enqueues all survive with distinct seqs, in memory and on
    /// disk. Guards review finding 8: the disk copy used to re-run the caller's
    /// closure and re-derive `seq` from whatever the file happened to hold, so
    /// racing enqueues could persist duplicate or reordered seqs.
    ///
    /// It does NOT prove the per-session persist lock. Copying a whole snapshot
    /// makes write ordering load-bearing (a stale `[a]` landing after `[a, b]`
    /// would drop `b`), and the lock exists to make that ordering guaranteed
    /// rather than incidental, but 32-way concurrency here never reordered the
    /// two `Storage::update` calls, so the lock is defensive and unproven.
    #[cfg(feature = "serve")]
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
        let service = crate::server::test_support::build_test_app_state(vec![inst])
            .session_service
            .clone();

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
        // The disk-side check is the one that fails on finding 8: re-deriving
        // `seq` per copy let two rows persist the same one.
        let mut disk_seqs: Vec<u64> = persisted.iter().map(|q| q.seq).collect();
        disk_seqs.sort_unstable();
        disk_seqs.dedup();
        assert_eq!(disk_seqs.len(), 32, "no two persisted rows share a seq");
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn queue_store_enqueue_edit_remove_clear() {
        let mut inst = Instance::new("queue", "/tmp/aoe-queue-project");
        inst.id = "sess-q".to_string();
        inst.view = crate::session::View::Structured;
        let service = crate::server::test_support::build_test_app_state(vec![inst])
            .session_service
            .clone();

        // Enqueue two: seqs are assigned monotonically and the snapshot is
        // ordered by seq.
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

        // Re-enqueue by the same id is an idempotent update, not a duplicate:
        // an optimistic client retry cannot double-queue.
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
        // Blanking a text-only row is refused and leaves the text intact. The
        // drain can neither deliver nor retire such a row, so accepting the
        // edit would wedge the queue behind it (and pin the worker alive,
        // since the idle reaper skips a session that has one).
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

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn wake_dormant_for_queue_drain_clears_only_when_dormant() {
        // A session the idle reaper auto-stopped: dormant, so the resume pass
        // skips it. Wake-on-drain must clear the marker so the queue can drain.
        let mut dormant = Instance::new("queue", "/tmp/aoe-queue-dormant");
        dormant.id = "sess-dormant".to_string();
        dormant.view = crate::session::View::Structured;
        dormant.mark_idle_dormant();
        assert!(dormant.is_idle_dormant());

        // A live/idle session that is not dormant must be left untouched: the
        // wake only ever clears the marker, never sets it.
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

    #[cfg(feature = "serve")]
    #[test]
    fn queue_drain_batch_splits_on_clear_boundary() {
        use crate::acp::state::QueuedPromptEntry;
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

        // No clear anywhere: the whole queue combines and empty-text rows are
        // skipped (so no stray blank-line separators).
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

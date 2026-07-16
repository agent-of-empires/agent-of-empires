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

/// Result of matching a plugin create request against the persisted sessions.
enum IdempotentMatch {
    /// Same plugin, key, and payload: return this existing session.
    Same(Box<Instance>),
    /// Same plugin and key, different payload: refuse.
    Conflict,
    /// No session carries this plugin/key pair.
    None,
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
    /// In-flight plugin creates keyed by `(plugin_id, idempotency_key)`.
    /// Sync mutex: critical sections are tiny and never span an `await`.
    // ponytail: one daemon process is the only sessions.json writer, so a
    // process-local registry closes the duplicate-create race; a cross-process
    // reservation store only becomes necessary if that assumption changes.
    create_in_flight: std::sync::Mutex<HashMap<(String, String), CreateInFlight>>,
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
    /// Pre-publish: reserving the resume slot failed (includes
    /// `SupervisorError::CapacityFull`). Nothing was published.
    ResumeFailed(crate::acp::supervisor::SupervisorError),
    /// Post-publish: the respawn kicked by this call did not finish within
    /// `send_prompt`'s wait window (slow sandbox / spawn). The worker is
    /// still coming; retryable. See #1748.
    WorkerNotReady,
    /// Post-publish: the forward to the agent failed.
    Send(crate::acp::supervisor::SupervisorError),
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
    ) -> Self {
        Self {
            instances,
            instance_locks,
            file_watch,
            telemetry_session_creates,
            acp_supervisor,
            create_in_flight: std::sync::Mutex::new(HashMap::new()),
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
    ) -> anyhow::Result<(SpawnOutcome, bool)> {
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
        id: &str,
        text: &str,
        attachments: &[crate::acp::event_store::AttachmentBlob],
        woke_idle_dormant: bool,
    ) -> Result<(), SendTurnError> {
        use crate::server::acp_reconciler::ResumeTrigger;
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
        // Publish the user's prompt into the event stream BEFORE forwarding
        // to the agent so the replay buffer / on-disk store captures it
        // even if the agent forward fails. The frontend treats UserPromptSent
        // as authoritative and dedupes against its own optimistic row.
        self.acp_supervisor
            .publish_user_prompt_with_attachments(id, text.to_string(), attachments)
            .await;
        match self.acp_supervisor.send_prompt(id, text, attachments).await {
            Ok(()) => Ok(()),
            // Intentional override of the canonical UnknownSession 404: the
            // respawn we kicked above did not finish within `send_prompt`'s
            // wait window. See the `WorkerNotReady` variant doc.
            Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) if needs_resume => {
                Err(SendTurnError::WorkerNotReady)
            }
            Err(e) => Err(SendTurnError::Send(e)),
        }
    }

    /// Same lazy per-instance mutex registry as `AppState::instance_lock`;
    /// both operate on the shared map, so a lock taken through either handle
    /// excludes the other.
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
            scratch: false,
            trust_hooks: None,
            custom_instruction: None,
            profile: "default".to_string(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
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
}

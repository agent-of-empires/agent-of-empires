//! One-shot native conversation establishment, independent of runtime readiness.

use crate::acp::state::{NativeBindingState, NativeEventContext};
use crate::session::{Instance, Storage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptId(Uuid);

impl std::fmt::Display for AttemptId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for AttemptId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityPhase {
    Pending,
    Blocked,
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityKey {
    pub(crate) id: AttemptId,
    pub(crate) phase: AuthorityPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum EstablishmentTarget {
    New,
    Load { session_id: String, import: bool },
    Fork { session_id: String },
}

impl EstablishmentTarget {
    pub(crate) fn method(&self) -> &'static str {
        match self {
            Self::New => "session/new",
            Self::Load { .. } => "session/load",
            Self::Fork { .. } => "session/fork",
        }
    }

    pub(crate) fn accepts_request(&self, method: &str, request: &serde_json::Value) -> bool {
        if method != self.method() {
            return false;
        }
        match self {
            Self::New => request.get("sessionId").is_none(),
            Self::Load { session_id, .. } | Self::Fork { session_id } => {
                request.get("sessionId").and_then(serde_json::Value::as_str)
                    == Some(session_id.as_str())
            }
        }
    }

    pub(crate) fn resulting_session_id<'a>(
        &'a self,
        result: &'a serde_json::Value,
    ) -> Result<&'a str, EstablishmentRefusal> {
        match self {
            Self::Load { session_id, .. } => {
                let result = result
                    .as_object()
                    .ok_or(EstablishmentRefusal::Unconfirmed)?;
                if result
                    .get("sessionId")
                    .is_some_and(|returned| returned.as_str() != Some(session_id.as_str()))
                {
                    return Err(EstablishmentRefusal::Superseded);
                }
                Ok(session_id)
            }
            Self::New | Self::Fork { .. } => result
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or(EstablishmentRefusal::Unconfirmed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationStage {
    // Admitted records consumed intent, not a successful B admission or readiness.
    Preparing,
    PreparingAdmitted,
    Ready,
    Admitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EstablishmentOrigin {
    Creation { stage: CreationStage },
    Selection,
    Same,
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstablishmentAttempt {
    pub(crate) id: AttemptId,
    pub(crate) profile: String,
    pub(crate) session_id: String,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) source_context: NativeEventContext,
    pub(crate) context: NativeEventContext,
    pub(crate) generation: u64,
    pub(crate) preimage: NativeBindingState,
    pub(crate) destination: NativeBindingState,
    pub(crate) target: EstablishmentTarget,
    pub(crate) origin: EstablishmentOrigin,
    pub(crate) producer: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstablishmentBlockReason {
    NotIssued,
    Unsupported,
    Rejected,
    OutcomeUnknown,
    ReplayUnconfirmed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcpEstablishment {
    Pending {
        attempt: Box<EstablishmentAttempt>,
    },
    Blocked {
        attempt: Box<EstablishmentAttempt>,
        reason: EstablishmentBlockReason,
    },
    Applied {
        attempt_id: AttemptId,
    },
}

impl AcpEstablishment {
    pub(crate) fn key(&self) -> AuthorityKey {
        match self {
            Self::Pending { attempt } => AuthorityKey {
                id: attempt.id,
                phase: AuthorityPhase::Pending,
            },
            Self::Blocked { attempt, .. } => AuthorityKey {
                id: attempt.id,
                phase: AuthorityPhase::Blocked,
            },
            Self::Applied { attempt_id } => AuthorityKey {
                id: *attempt_id,
                phase: AuthorityPhase::Applied,
            },
        }
    }

    pub(crate) fn unresolved(&self) -> bool {
        !matches!(self, Self::Applied { .. })
    }

    pub(crate) fn attempt(&self) -> Option<&EstablishmentAttempt> {
        match self {
            Self::Pending { attempt } | Self::Blocked { attempt, .. } => Some(attempt),
            Self::Applied { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstablishmentExpected {
    pub(crate) context: NativeEventContext,
    pub(crate) choice: NativeBindingState,
    pub(crate) authority: Option<AcpEstablishment>,
}

impl EstablishmentExpected {
    pub(crate) fn capture(row: &Instance, storage: &Storage) -> anyhow::Result<Self> {
        Ok(Self {
            context: NativeEventContext::capture(row, storage)?,
            choice: NativeBindingState::capture(row),
            authority: row.acp_establishment.clone(),
        })
    }

    pub(crate) fn matches(&self, row: &Instance, storage: &Storage) -> anyhow::Result<bool> {
        Ok(self.context == NativeEventContext::capture(row, storage)?
            && self.choice.matches(row)
            && self.authority == row.acp_establishment)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstablishmentRecoveryAction {
    Same,
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstablishmentRecovery {
    pub(crate) expected: EstablishmentExpected,
    pub(crate) action: EstablishmentRecoveryAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeEstablishmentOutcome {
    Succeeded { session_id: String },
    Failed { reason: EstablishmentBlockReason },
}

/// Completeness of the original native response boundary, not a new attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReplayClosure {
    pub(crate) notifications: u64,
    pub(crate) closed: bool,
    pub(crate) lost: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEstablishmentReceipt {
    pub(crate) attempt: std::sync::Arc<EstablishmentAttempt>,
    pub(crate) owner: crate::acp::runner_lifecycle::RunnerIdentity,
    pub(crate) store: Option<crate::session::ExecutionBinding>,
    pub(crate) outcome: NativeEstablishmentOutcome,
    pub(crate) replay: NativeReplayClosure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstablishmentPostimage {
    pub(crate) attempt_id: AttemptId,
    pub(crate) context: NativeEventContext,
    pub(crate) choice: NativeBindingState,
}

/// Frozen current-session observation permission, not a turn or request ACK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionPermission {
    pub(crate) postimage: EstablishmentPostimage,
    pub(crate) generation: u64,
    pub(crate) profile_identity: (u64, u64),
    pub(crate) owner: crate::acp::runner_lifecycle::RunnerIdentity,
}

/// Compact source evidence copied before native ingress can queue or await.
/// Only the authenticated runner transport supplies this observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionStamp {
    pub(crate) context: NativeEventContext,
    pub(crate) generation: u64,
    pub(crate) profile_identity: (u64, u64),
}

/// Positive, pre-queue authority for one still-Pending original handshake.
/// This is never interchangeable with an Applied current-session stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginalRequestStamp {
    pub(crate) attempt_id: AttemptId,
    pub(crate) context: NativeEventContext,
    pub(crate) generation: u64,
    pub(crate) profile_identity: (u64, u64),
}

impl OriginalRequestStamp {
    pub(crate) fn capture(
        receipt: &NativeEstablishmentReceipt,
        row: &Instance,
        storage: &Storage,
        reported_session: Option<&str>,
    ) -> anyhow::Result<Self> {
        row.ensure_startable()?;
        if let (EstablishmentTarget::Load { session_id, .. }, Some(reported)) =
            (&receipt.attempt.target, reported_session)
        {
            anyhow::ensure!(session_id == reported, EstablishmentRefusal::Superseded);
        }
        anyhow::ensure!(
            row.is_structured()
                && row.terminal_handoff.is_none()
                && !matches!(
                    row.status,
                    crate::session::Status::Stopped | crate::session::Status::Deleting
                )
                && receipt.attempt.producer.is_some()
                && receipt.owner.birth_fingerprint.is_some()
                && receipt.attempt.generation == row.lifecycle_generation
                && receipt.attempt.allows_effects()
                && receipt.attempt.matches_pending(row, storage)?,
            EstablishmentRefusal::Superseded
        );
        Ok(Self {
            attempt_id: receipt.attempt.id,
            context: NativeEventContext::capture(row, storage)?,
            generation: row.lifecycle_generation,
            profile_identity: storage.bound_profile_identity()?,
        })
    }
}

/// Identity of one actual admission, unrelated to the native JSON-RPC id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeOperationId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOperationCorrelation {
    pub(crate) operation_id: NativeOperationId,
    pub(crate) request_nonce: Option<Uuid>,
}

impl NativeOperationCorrelation {
    pub(crate) fn new(request_nonce: Option<Uuid>) -> Self {
        Self {
            operation_id: NativeOperationId(Uuid::new_v4()),
            request_nonce,
        }
    }
}

/// One business operation retains both its admission identity and permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOperationPermission {
    pub(crate) correlation: NativeOperationCorrelation,
    pub(crate) session: NativeSessionPermission,
}

impl NativeOperationPermission {
    pub(crate) async fn capture_owned(
        receipt: &NativeEstablishmentReceipt,
        owner: &crate::acp::runner_lifecycle::RunnerIdentity,
        correlation: NativeOperationCorrelation,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Self> {
        enum Wait {
            Lifecycle,
            Registry,
        }
        let storage = Storage::open_reference(&receipt.attempt.profile)?;
        let mut candidate: Option<NativeSessionPermission> = None;
        loop {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                crate::process::worker_registry::RegistryBusy
            );
            let wait = {
                if let Some(_native) =
                    storage.try_acquire_instance_lifecycle_lock(&receipt.attempt.session_id)?
                {
                    storage.verify_bound_profile()?;
                    let row = storage
                        .load_instances_complete()?
                        .into_iter()
                        .find(|row| row.id == receipt.attempt.session_id)
                        .ok_or(crate::session::SessionGone)?;
                    if candidate.is_none() {
                        candidate = Some(NativeSessionPermission::capture(
                            receipt, &row, &storage, owner,
                        )?);
                    }
                    let permission = candidate
                        .as_ref()
                        .expect("captured under lifecycle exclusion");
                    anyhow::ensure!(
                        permission.matches(receipt, &row, &storage)?,
                        EstablishmentRefusal::Superseded
                    );
                    if let Some(registry) = crate::process::worker_registry::try_registry_lock(
                        &receipt.attempt.session_id,
                    )? {
                        permission.check_owner(receipt, &registry)?;
                        storage.verify_bound_profile()?;
                        return Ok(Self {
                            correlation,
                            session: candidate.expect("validated original candidate"),
                        });
                    }
                    Wait::Registry
                } else {
                    Wait::Lifecycle
                }
            };
            match wait {
                Wait::Lifecycle => {
                    tokio::time::sleep_until(
                        deadline
                            .min(tokio::time::Instant::now() + std::time::Duration::from_millis(2)),
                    )
                    .await
                }
                Wait::Registry => drop(
                    crate::process::worker_registry::registry_lock_until(
                        &receipt.attempt.session_id,
                        deadline,
                    )
                    .await?,
                ),
            }
        }
    }

    pub(crate) fn matches(
        &self,
        receipt: &NativeEstablishmentReceipt,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        self.session.matches(receipt, row, storage)
    }

    pub(crate) fn effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.session.effect_guard(receipt)
    }

    pub(crate) fn try_effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.session.try_effect_guard(receipt)
    }
}

/// Every native request declares its frozen scope. Observation never grants a
/// business effect, and Original cannot cross its original lifecycle context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeRequestScope {
    Operation {
        permission: std::sync::Arc<NativeOperationPermission>,
    },
    /// Bound to a genuine native callback by the existing reverse request ticket.
    /// Callback identity is not an originating prompt or user-operation correlation.
    Callback {
        permission: std::sync::Arc<NativeSessionPermission>,
    },
    /// Current session observation only; never a client business request or callback ticket.
    Session {
        permission: std::sync::Arc<NativeSessionPermission>,
    },
    Original {
        attempt: std::sync::Arc<EstablishmentAttempt>,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        store: Option<std::sync::Arc<crate::session::ExecutionBinding>>,
        operation: Option<NativeOperationCorrelation>,
    },
    Observation,
}

impl NativeRequestScope {
    pub(crate) fn owner(&self) -> Option<crate::acp::runner_lifecycle::RunnerIdentity> {
        match self {
            Self::Operation { permission } => Some(permission.session.owner),
            Self::Callback { permission } | Self::Session { permission } => Some(permission.owner),
            Self::Original { owner, .. } => Some(*owner),
            Self::Observation => None,
        }
    }

    pub(crate) fn correlation(&self) -> Option<NativeOperationCorrelation> {
        match self {
            Self::Operation { permission } => Some(permission.correlation),
            Self::Original { operation, .. } => *operation,
            Self::Callback { .. } | Self::Session { .. } | Self::Observation => None,
        }
    }

    pub(crate) fn origin(
        &self,
        receipt: Option<&NativeEstablishmentReceipt>,
    ) -> Option<crate::acp::state::NativeEventOrigin> {
        use crate::acp::state::NativeEventOrigin;
        let session = match self {
            Self::Operation { permission } => &permission.session,
            Self::Callback { permission } | Self::Session { permission } => permission,
            Self::Original { attempt, owner, .. } => {
                return NativeEventOrigin::owned_attempt(attempt, Some(*owner));
            }
            Self::Observation => return None,
        };
        let receipt = receipt?;
        if session.postimage.attempt_id != receipt.attempt.id || session.owner != receipt.owner {
            return None;
        }
        let original = NativeEventOrigin::owned_attempt(&receipt.attempt, Some(session.owner))?;
        Some(NativeEventOrigin {
            context: session.postimage.context,
            generation: session.generation,
            ..original
        })
    }

    pub(crate) fn matches(
        &self,
        receipt: &NativeEstablishmentReceipt,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        match self {
            Self::Operation { permission } => permission.matches(receipt, row, storage),
            Self::Callback { permission } | Self::Session { permission } => {
                permission.matches(receipt, row, storage)
            }
            Self::Original {
                attempt,
                owner,
                store,
                ..
            } => {
                row.ensure_startable()?;
                if !row.is_structured()
                    || row.terminal_handoff.is_some()
                    || !attempt.allows_effects()
                    || attempt.as_ref() != receipt.attempt.as_ref()
                    || *owner != receipt.owner
                    || owner.birth_fingerprint.is_none()
                    || !receipt.same_store(store.as_deref())
                    || attempt.context != NativeEventContext::capture(row, storage)?
                {
                    return Ok(false);
                }
                if attempt.matches_pending(row, storage)? {
                    return Ok(true);
                }
                Ok(receipt.current_applied_postimage(row, storage, owner)?
                    == receipt.postimage(attempt.context)?)
            }
            Self::Observation => Ok(false),
        }
    }

    /// A synchronous callback effect, never a human/protocol continuation.
    pub(crate) fn effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.lock_effect_guard(receipt, false)
    }

    pub(crate) fn try_effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.lock_effect_guard(receipt, true)
    }

    fn lock_effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        if let Self::Operation { permission } = self {
            return if nonblocking {
                permission.try_effect_guard(receipt)
            } else {
                permission.effect_guard(receipt)
            };
        }
        if let Self::Callback { permission } | Self::Session { permission } = self {
            return if nonblocking {
                permission.try_effect_guard(receipt)
            } else {
                permission.effect_guard(receipt)
            };
        }
        let owner = self.owner().ok_or(EstablishmentRefusal::Unconfirmed)?;
        let storage = Storage::open_reference(&receipt.attempt.profile)?;
        let native = if nonblocking {
            storage
                .try_acquire_instance_lifecycle_lock(&receipt.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?
        } else {
            storage.acquire_instance_lifecycle_lock(&receipt.attempt.session_id)?
        };
        let row = storage
            .load_instances_complete()?
            .into_iter()
            .find(|row| row.id == receipt.attempt.session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            self.matches(receipt, &row, &storage)?,
            EstablishmentRefusal::Superseded
        );
        let registry =
            crate::process::worker_registry::try_registry_lock(&receipt.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            registry.stop()?.is_none()
                && owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(receipt.attempt.profile.as_str())
                && crate::process::process_birth_identity(owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok((native, registry))
    }
}

/// Private continuation evidence accompanies the frozen wire scope. Keeping the
/// original receipt here prevents a later result from borrowing a successor's
/// projection merely because the physical runner or native session id matches.
#[derive(Debug, Clone)]
pub(crate) struct NativeRequestContext {
    pub(crate) scope: NativeRequestScope,
    pub(crate) receipt: Option<std::sync::Arc<NativeEstablishmentReceipt>>,
    observation: Option<NativeSessionEffectCache>,
    callback: Option<NativeCallbackContext>,
}

#[derive(Debug, Clone)]
struct NativeSessionEffectCache {
    fence: std::sync::Arc<crate::session::NativeInputFence>,
    original: std::sync::Arc<EstablishmentPostimage>,
}

/// Genuine original reverse-request authority, shared with the existing ingress.
#[derive(Debug)]
pub(crate) struct NativeCallbackOriginal {
    pub(crate) attempt: std::sync::Arc<EstablishmentAttempt>,
    pub(crate) owner: crate::acp::runner_lifecycle::RunnerIdentity,
    pub(crate) store: Option<crate::session::ExecutionBinding>,
    pub(crate) postimage: std::sync::OnceLock<std::sync::Arc<EstablishmentPostimage>>,
    pub(crate) notification_scope:
        std::sync::Mutex<Option<(NativeSessionStamp, std::sync::Arc<NativeRequestContext>)>>,
}

impl NativeCallbackOriginal {
    pub(crate) fn matches_original(
        &self,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        row.ensure_startable()?;
        if !row.is_structured() || row.terminal_handoff.is_some() {
            return Ok(false);
        }
        if !self.attempt.allows_effects() || !self.attempt.matches_physical_source(row, storage)? {
            return Ok(false);
        }
        let pending = self.attempt.matches_pending_source(row, storage)?;
        let applied = self.postimage.get().is_some_and(|postimage| {
            postimage.attempt_id == self.attempt.id
                && postimage.context == self.attempt.context
                && postimage.choice.matches(row)
                && row.acp_establishment
                    == Some(AcpEstablishment::Applied {
                        attempt_id: self.attempt.id,
                    })
        });
        Ok(pending || applied)
    }

    pub(crate) fn check_owner(
        &self,
        registry: &crate::process::worker_registry::RegistryGuard<'_>,
    ) -> anyhow::Result<()> {
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            registry.stop()?.is_none()
                && self.owner.birth_fingerprint.is_some()
                && self.owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(self.attempt.profile.as_str())
                && crate::process::process_birth_identity(self.owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == self.owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NativeConstructorObservation {
    original: std::sync::Arc<NativeCallbackOriginal>,
    fence: std::sync::Arc<crate::session::NativeInputFence>,
    origin: crate::acp::state::NativeEventOrigin,
}

impl NativeConstructorObservation {
    pub(crate) fn capture(
        original: std::sync::Arc<NativeCallbackOriginal>,
        context: NativeEventContext,
        fence: std::sync::Arc<crate::session::NativeInputFence>,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        anyhow::ensure!(
            context == original.attempt.context,
            EstablishmentRefusal::Superseded
        );
        let origin = crate::acp::state::NativeEventOrigin::owned_attempt(
            &original.attempt,
            Some(original.owner),
        )
        .ok_or(EstablishmentRefusal::Unconfirmed)?;
        Ok(std::sync::Arc::new(Self {
            original,
            fence,
            origin,
        }))
    }

    pub(crate) fn origin(&self) -> crate::acp::state::NativeEventOrigin {
        self.origin
    }

    pub(crate) fn try_publication_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.lock_publication_guard(true)
    }

    pub(crate) fn publication_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.lock_publication_guard(false)
    }

    fn lock_publication_guard(
        &self,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        let (native, ()) = self
            .fence
            .lock_observation_project(
                |row, storage| {
                    row.ensure_startable()?;
                    anyhow::ensure!(
                        !matches!(
                            row.status,
                            crate::session::Status::Stopped | crate::session::Status::Deleting
                        ) && row.terminal_handoff.is_none()
                            && self.original.attempt.generation == row.lifecycle_generation
                            && self.original.attempt.context
                                == NativeEventContext::capture(row, storage)?
                            && self.original.matches_original(row, storage)?,
                        EstablishmentRefusal::Superseded
                    );
                    Ok(())
                },
                nonblocking,
            )?
            .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let registry =
            crate::process::worker_registry::try_registry_lock(&self.original.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        self.original.check_owner(&registry)?;
        Ok((native, registry))
    }
}

#[derive(Debug, Clone)]
struct NativeCallbackContext {
    original: std::sync::Arc<NativeCallbackOriginal>,
    stamp: Option<NativeSessionStamp>,
    original_stamp: Option<OriginalRequestStamp>,
    session_id: Option<std::sync::Arc<str>>,
    fence: std::sync::Arc<crate::session::NativeInputFence>,
}

#[derive(Clone, Copy)]
enum CallbackUse {
    Current,
    OriginalDecision,
    OriginalIo,
    OwnedResource,
}

impl NativeCallbackContext {
    fn guard(
        &self,
        purpose: CallbackUse,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        let current = matches!(purpose, CallbackUse::Current);
        let new_io = matches!(purpose, CallbackUse::OriginalIo);
        let original_decision = matches!(purpose, CallbackUse::OriginalDecision);
        let project = |row: &Instance, storage: &Storage| {
            anyhow::ensure!(
                self.original.matches_original(row, storage)?,
                EstablishmentRefusal::Superseded
            );
            if original_decision {
                let stamp = self
                    .original_stamp
                    .ok_or(EstablishmentRefusal::Unconfirmed)?;
                anyhow::ensure!(
                    stamp.attempt_id == self.original.attempt.id
                        && stamp.context == NativeEventContext::capture(row, storage)?
                        && stamp.generation == row.lifecycle_generation
                        && stamp.profile_identity == storage.bound_profile_identity()?
                        && !matches!(
                            row.status,
                            crate::session::Status::Stopped | crate::session::Status::Deleting
                        ),
                    EstablishmentRefusal::Superseded
                );
                // A New callback does not select an id. Load must match its
                // already-known id; global forms remain bound to this peer.
                if let (EstablishmentTarget::Load { session_id, .. }, Some(reported)) =
                    (&self.original.attempt.target, self.session_id.as_deref())
                {
                    anyhow::ensure!(session_id == reported, EstablishmentRefusal::Superseded);
                }
            }
            if new_io {
                anyhow::ensure!(
                    self.session_id
                        .as_deref()
                        .is_some_and(|sid| !sid.is_empty()),
                    EstablishmentRefusal::Unconfirmed
                );
            }
            if let (true, Some(postimage), Some(session_id)) = (
                current || new_io,
                self.original.postimage.get(),
                self.session_id.as_deref(),
            ) {
                anyhow::ensure!(
                    postimage.choice.acp_session_id.as_deref() == Some(session_id),
                    EstablishmentRefusal::Superseded
                );
            }
            if current || new_io {
                anyhow::ensure!(
                    !matches!(
                        row.status,
                        crate::session::Status::Stopped | crate::session::Status::Deleting
                    ),
                    EstablishmentRefusal::Superseded
                );
            }
            if current {
                anyhow::ensure!(
                    row.acp_establishment
                        == Some(AcpEstablishment::Applied {
                            attempt_id: self.original.attempt.id,
                        }),
                    EstablishmentRefusal::Unconfirmed
                );
                let stamp = self.stamp.ok_or(EstablishmentRefusal::Unconfirmed)?;
                anyhow::ensure!(
                    stamp.generation == row.lifecycle_generation
                        && stamp.context == NativeEventContext::capture(row, storage)?
                        && stamp.profile_identity == storage.bound_profile_identity()?,
                    EstablishmentRefusal::Superseded
                );
            }
            Ok(())
        };
        let (native, ()) = match purpose {
            CallbackUse::Current | CallbackUse::OriginalDecision => {
                self.fence.lock_observation_project(project, nonblocking)?
            }
            CallbackUse::OriginalIo if !nonblocking => {
                self.fence.lock_original_history_project(project)?
            }
            CallbackUse::OriginalIo | CallbackUse::OwnedResource => {
                self.fence.try_lock_original_history_project(project)?
            }
        }
        .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let registry =
            crate::process::worker_registry::try_registry_lock(&self.original.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        self.original.check_owner(&registry)?;
        Ok((native, registry))
    }
}

impl NativeRequestContext {
    pub(crate) fn new(
        scope: NativeRequestScope,
        receipt: Option<std::sync::Arc<NativeEstablishmentReceipt>>,
    ) -> Self {
        Self {
            scope,
            receipt,
            observation: None,
            callback: None,
        }
    }

    pub(crate) fn session_observation(
        receipt: std::sync::Arc<NativeEstablishmentReceipt>,
        original: std::sync::Arc<EstablishmentPostimage>,
        stamp: NativeSessionStamp,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            original.as_ref() == &receipt.postimage(receipt.attempt.context)?,
            EstablishmentRefusal::Unconfirmed
        );
        let permission = NativeSessionPermission::from_stamp(&receipt, &original, stamp)?;
        let fence = crate::session::NativeInputFence::capture_route(
            &receipt.attempt.profile,
            &receipt.attempt.session_id,
            receipt.attempt.created_at,
            stamp.generation,
            false,
            Some(stamp.profile_identity),
        )?;
        Ok(Self {
            scope: NativeRequestScope::Session {
                permission: std::sync::Arc::new(permission),
            },
            receipt: Some(receipt),
            observation: Some(NativeSessionEffectCache { fence, original }),
            callback: None,
        })
    }

    fn observation_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
        cache: &NativeSessionEffectCache,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        let NativeRequestScope::Session { permission } = &self.scope else {
            anyhow::bail!(EstablishmentRefusal::Unconfirmed);
        };
        let (native, ()) = cache
            .fence
            .lock_observation_project(
                |row, storage| {
                    NativeSessionPermission::admit_row(row)?;
                    anyhow::ensure!(
                        permission.owner == receipt.owner
                            && permission.generation == row.lifecycle_generation
                            && permission.profile_identity == storage.bound_profile_identity()?
                            && permission.postimage.attempt_id == receipt.attempt.id
                            && permission.postimage.context
                                == NativeEventContext::capture(row, storage)?
                            && permission.postimage.choice == cache.original.choice
                            && permission.postimage.choice.matches(row)
                            && receipt.attempt.matches_physical_source(row, storage)?,
                        EstablishmentRefusal::Superseded
                    );
                    Ok(())
                },
                nonblocking,
            )?
            .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let registry =
            crate::process::worker_registry::try_registry_lock(&receipt.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        NativeSessionPermission::check_stamp_owner(receipt, &registry)?;
        Ok((native, registry))
    }

    /// Only the authenticated reverse ticket may call this constructor.
    pub(crate) fn callback(
        original: std::sync::Arc<NativeCallbackOriginal>,
        stamp: Option<NativeSessionStamp>,
        original_stamp: Option<OriginalRequestStamp>,
        session_id: Option<std::sync::Arc<str>>,
    ) -> anyhow::Result<Self> {
        let fence = crate::session::NativeInputFence::capture_route(
            &original.attempt.profile,
            &original.attempt.session_id,
            original.attempt.created_at,
            stamp
                .map(|stamp| stamp.generation)
                .or_else(|| original_stamp.map(|stamp| stamp.generation))
                .unwrap_or(original.attempt.generation),
            false,
            stamp
                .map(|stamp| stamp.profile_identity)
                .or_else(|| original_stamp.map(|stamp| stamp.profile_identity)),
        )?;
        anyhow::ensure!(
            !(stamp.is_some() && original_stamp.is_some()),
            EstablishmentRefusal::Unconfirmed
        );
        Ok(Self {
            // This wire value alone deliberately grants no native input.
            scope: NativeRequestScope::Observation,
            receipt: None,
            observation: None,
            callback: Some(NativeCallbackContext {
                original,
                stamp,
                original_stamp,
                session_id,
                fence,
            }),
        })
    }

    pub(crate) fn try_owned_resource_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.callback
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?
            .guard(CallbackUse::OwnedResource, true)
    }

    pub(crate) fn original_io_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.callback
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?
            .guard(CallbackUse::OriginalIo, false)
    }

    pub(crate) fn try_original_io_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.callback
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?
            .guard(CallbackUse::OriginalIo, true)
    }

    pub(crate) fn is_original_decision(&self) -> bool {
        self.callback
            .as_ref()
            .is_some_and(|callback| callback.original_stamp.is_some())
    }

    pub(crate) fn try_decision_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.decision_guard(true)
    }

    pub(crate) fn decision_guard(
        &self,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        let callback = self
            .callback
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let purpose = if callback.original_stamp.is_some() {
            CallbackUse::OriginalDecision
        } else {
            CallbackUse::Current
        };
        callback.guard(purpose, nonblocking)
    }

    pub(crate) fn same_callback_source(&self, other: &Self) -> bool {
        match (&self.callback, &other.callback) {
            (Some(left), Some(right)) => {
                left.original.attempt.same_source(&right.original.attempt)
                    && left.original.owner == right.original.owner
                    && left.original.store == right.original.store
                    && left
                        .original
                        .store
                        .as_ref()
                        .and_then(|binding| binding.exported_default_store.as_ref())
                        == right
                            .original
                            .store
                            .as_ref()
                            .and_then(|binding| binding.exported_default_store.as_ref())
                    && left.session_id == right.session_id
            }
            _ => false,
        }
    }

    pub(crate) fn effect_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        if let Some(callback) = &self.callback {
            return callback.guard(CallbackUse::Current, false);
        }
        let receipt = self
            .receipt
            .as_deref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        if let Some(cache) = &self.observation {
            return self.observation_guard(receipt, cache, false);
        }
        self.scope.effect_guard(receipt)
    }

    pub(crate) fn try_effect_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        if let Some(callback) = &self.callback {
            return callback.guard(CallbackUse::Current, true);
        }
        let receipt = self
            .receipt
            .as_deref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        if let Some(cache) = &self.observation {
            return self.observation_guard(receipt, cache, true);
        }
        self.scope.try_effect_guard(receipt)
    }

    pub(crate) fn with_original_evidence(
        &self,
        receipt: std::sync::Arc<NativeEstablishmentReceipt>,
    ) -> anyhow::Result<Self> {
        let NativeRequestScope::Original {
            attempt,
            owner,
            store,
            ..
        } = &self.scope
        else {
            anyhow::bail!(EstablishmentRefusal::Unconfirmed);
        };
        anyhow::ensure!(
            attempt.as_ref() == receipt.attempt.as_ref()
                && *owner == receipt.owner
                && receipt.same_store(store.as_deref())
                && receipt.session_id().is_some_and(|sid| !sid.is_empty())
                && receipt.replay.closed
                && !receipt.replay.lost,
            EstablishmentRefusal::Unconfirmed
        );
        Ok(Self::new(self.scope.clone(), Some(receipt)))
    }

    pub(crate) fn correlation(&self) -> Option<NativeOperationCorrelation> {
        self.scope.correlation()
    }

    pub(crate) fn has_current_callback(&self) -> bool {
        self.callback
            .as_ref()
            .is_some_and(|callback| callback.stamp.is_some())
    }

    pub(crate) fn origin(&self) -> Option<crate::acp::state::NativeEventOrigin> {
        if let Some(callback) = &self.callback {
            return crate::acp::state::NativeEventOrigin::owned_attempt(
                &callback.original.attempt,
                Some(callback.original.owner),
            );
        }
        self.scope.origin(self.receipt.as_deref())
    }
}

/// A resource START permission captured for that purpose at Native framing.
/// This is not a callback ticket, public scope, or originating user operation.
#[derive(Debug, Clone)]
pub(crate) struct NativeResourceStartContext {
    context: std::sync::Arc<NativeRequestContext>,
    session_id: agent_client_protocol::schema::v1::SessionId,
}

impl NativeResourceStartContext {
    pub(crate) fn from_frame(
        context: std::sync::Arc<NativeRequestContext>,
        session_id: agent_client_protocol::schema::v1::SessionId,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            matches!(&context.scope, NativeRequestScope::Session { .. })
                && context.observation.is_some()
                && context
                    .receipt
                    .as_deref()
                    .and_then(NativeEstablishmentReceipt::session_id)
                    == Some(session_id.0.as_ref()),
            EstablishmentRefusal::Unconfirmed
        );
        Ok(Self {
            context,
            session_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NativeResourceLocation {
    Host,
    Container { runtime: String, container: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeWorkflowSelector {
    pub(crate) run_id: String,
    pub(crate) transcript_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NativeResourceSelector {
    Agent {
        agent_id: String,
        tool_call_id: String,
        output_file: String,
        source: NativeResourceLocation,
    },
    Task {
        task_id: String,
        tool_call_id: Option<String>,
        session_id: String,
        claude_home: std::path::PathBuf,
        workflow: Option<NativeWorkflowSelector>,
    },
    WorkflowChild {
        parent_allocation: Uuid,
        run_id: String,
        task_id: String,
        agent_id: String,
        output_file: String,
        source: NativeResourceLocation,
    },
}

/// Durable private evidence of one genuinely admitted tracking allocation.
/// Payload paths never enter public frame metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeResourceWitness {
    pub(crate) allocation: Uuid,
    pub(crate) selector: std::sync::Arc<NativeResourceSelector>,
    pub(crate) origin: crate::acp::state::NativeEventOrigin,
    pub(crate) profile: String,
    pub(crate) session_id: String,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) native_session_id: agent_client_protocol::schema::v1::SessionId,
    pub(crate) postimage: std::sync::Arc<EstablishmentPostimage>,
    pub(crate) admitted: NativeSessionStamp,
}

impl NativeResourceWitness {
    pub(crate) fn matches_agent_launch(
        &self,
        agent: &str,
        tool: &str,
        path: &str,
        workflow_run: Option<&str>,
    ) -> bool {
        match self.selector.as_ref() {
            NativeResourceSelector::Agent {
                agent_id,
                tool_call_id,
                output_file,
                ..
            } => agent_id == agent && tool_call_id == tool && output_file == path,
            NativeResourceSelector::WorkflowChild {
                agent_id,
                output_file,
                run_id,
                ..
            } => agent_id == agent && output_file == path && workflow_run == Some(run_id.as_str()),
            _ => false,
        }
    }

    pub(crate) fn matches_task_launch(
        &self,
        task: &str,
        tool: Option<&str>,
        workflow: Option<&crate::acp::state::WorkflowLaunch>,
    ) -> bool {
        match self.selector.as_ref() {
            NativeResourceSelector::Task {
                task_id,
                tool_call_id,
                session_id,
                workflow: selected,
                ..
            } => {
                task_id == task
                    && tool_call_id.as_deref() == tool
                    && session_id == self.native_session_id.0.as_ref()
                    && match (selected, workflow) {
                        (None, None) => true,
                        (Some(selected), Some(workflow)) => {
                            selected.run_id == workflow.run_id
                                && selected.transcript_dir == workflow.transcript_dir
                        }
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
enum NativeResourceSource {
    Current(std::sync::Arc<NativeResourceStartContext>),
    Original(std::sync::Arc<NativeResourceWitness>),
}

/// Lives in the existing tracking allocation, not another resource registry.
#[derive(Debug)]
pub(crate) struct NativeResourceAuthority {
    witness: Option<std::sync::Arc<NativeResourceWitness>>,
    selector: std::sync::Arc<NativeResourceSelector>,
    source: NativeResourceSource,
    fence: std::sync::Arc<crate::session::NativeInputFence>,
    started: std::sync::atomic::AtomicBool,
    workflow_root: std::sync::OnceLock<std::path::PathBuf>,
}

impl NativeResourceAuthority {
    pub(crate) fn start(
        start: std::sync::Arc<NativeResourceStartContext>,
        selector: NativeResourceSelector,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        let NativeRequestScope::Session { .. } = &start.context.scope else {
            anyhow::bail!(EstablishmentRefusal::Unconfirmed);
        };
        let cache = start
            .context
            .observation
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        if let NativeResourceSelector::Task { session_id, .. } = &selector {
            anyhow::ensure!(
                session_id == start.session_id.0.as_ref(),
                EstablishmentRefusal::Superseded
            );
        }
        Ok(std::sync::Arc::new(Self {
            witness: None,
            selector: std::sync::Arc::new(selector),
            source: NativeResourceSource::Current(start.clone()),
            fence: std::sync::Arc::clone(&cache.fence),
            started: std::sync::atomic::AtomicBool::new(false),
            workflow_root: std::sync::OnceLock::new(),
        }))
    }

    /// Called at the existing map's real tracking-allocation insertion.
    /// Adoption retains the original logical resource ID, not a new operation.
    pub(crate) fn for_tracking_allocation(
        self: &std::sync::Arc<Self>,
        allocation: Uuid,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        if self.witness.is_some() {
            return Ok(std::sync::Arc::clone(self));
        }
        let witness = match &self.source {
            NativeResourceSource::Current(start) => {
                let NativeRequestScope::Session { permission } = &start.context.scope else {
                    anyhow::bail!(EstablishmentRefusal::Unconfirmed);
                };
                let receipt = start
                    .context
                    .receipt
                    .as_deref()
                    .ok_or(EstablishmentRefusal::Unconfirmed)?;
                let cache = start
                    .context
                    .observation
                    .as_ref()
                    .ok_or(EstablishmentRefusal::Unconfirmed)?;
                NativeResourceWitness {
                    allocation,
                    selector: std::sync::Arc::clone(&self.selector),
                    origin: crate::acp::state::NativeEventOrigin::owned_attempt(
                        &receipt.attempt,
                        Some(receipt.owner),
                    )
                    .ok_or(EstablishmentRefusal::Unconfirmed)?,
                    profile: receipt.attempt.profile.clone(),
                    session_id: receipt.attempt.session_id.clone(),
                    created_at: receipt.attempt.created_at,
                    native_session_id: start.session_id.clone(),
                    postimage: std::sync::Arc::clone(&cache.original),
                    admitted: NativeSessionStamp {
                        context: permission.postimage.context,
                        generation: permission.generation,
                        profile_identity: permission.profile_identity,
                    },
                }
            }
            NativeResourceSource::Original(parent) => NativeResourceWitness {
                allocation,
                selector: std::sync::Arc::clone(&self.selector),
                origin: parent.origin,
                profile: parent.profile.clone(),
                session_id: parent.session_id.clone(),
                created_at: parent.created_at,
                native_session_id: parent.native_session_id.clone(),
                postimage: std::sync::Arc::clone(&parent.postimage),
                admitted: parent.admitted,
            },
        };
        Ok(std::sync::Arc::new(Self {
            witness: Some(std::sync::Arc::new(witness)),
            selector: std::sync::Arc::clone(&self.selector),
            source: self.source.clone(),
            fence: std::sync::Arc::clone(&self.fence),
            started: std::sync::atomic::AtomicBool::new(
                self.started.load(std::sync::atomic::Ordering::Acquire),
            ),
            workflow_root: std::sync::OnceLock::new(),
        }))
    }

    pub(crate) fn adopt(
        witness: std::sync::Arc<NativeResourceWitness>,
        receipt: &NativeEstablishmentReceipt,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        anyhow::ensure!(
            crate::acp::state::NativeEventOrigin::owned_attempt(
                &receipt.attempt,
                Some(receipt.owner)
            ) == Some(witness.origin)
                && receipt.attempt.profile == witness.profile
                && receipt.attempt.session_id == witness.session_id
                && receipt.attempt.created_at == witness.created_at
                && receipt.session_id() == Some(witness.native_session_id.0.as_ref())
                && receipt.replay.closed
                && !receipt.replay.lost
                && witness.postimage.as_ref() == &receipt.postimage(witness.origin.context)?,
            EstablishmentRefusal::Unconfirmed
        );
        let fence = crate::session::NativeInputFence::capture_route(
            &witness.profile,
            &witness.session_id,
            witness.created_at,
            witness.admitted.generation,
            false,
            Some(witness.admitted.profile_identity),
        )?;
        Ok(std::sync::Arc::new(Self {
            selector: std::sync::Arc::clone(&witness.selector),
            source: NativeResourceSource::Original(std::sync::Arc::clone(&witness)),
            witness: Some(witness),
            fence,
            started: std::sync::atomic::AtomicBool::new(true),
            workflow_root: std::sync::OnceLock::new(),
        }))
    }

    pub(crate) fn selector(&self) -> &NativeResourceSelector {
        &self.selector
    }

    pub(crate) fn same_resource(&self, other: &Self) -> bool {
        let identity = |authority: &Self| {
            authority
                .witness
                .as_ref()
                .map(|witness| witness.origin)
                .or_else(|| match &authority.source {
                    NativeResourceSource::Current(start) => {
                        start.context.receipt.as_deref().and_then(|receipt| {
                            crate::acp::state::NativeEventOrigin::owned_attempt(
                                &receipt.attempt,
                                Some(receipt.owner),
                            )
                        })
                    }
                    NativeResourceSource::Original(parent) => Some(parent.origin),
                })
        };
        self.selector == other.selector && identity(self) == identity(other)
    }

    pub(crate) fn witness(&self) -> Option<std::sync::Arc<NativeResourceWitness>> {
        if self.started.load(std::sync::atomic::Ordering::Acquire) {
            self.witness.clone()
        } else {
            None
        }
    }

    fn original_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        let witness = self
            .witness
            .as_deref()
            .or_else(|| match &self.source {
                NativeResourceSource::Original(parent) => Some(parent.as_ref()),
                NativeResourceSource::Current(_) => None,
            })
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let (native, ()) = self
            .fence
            .lock_original_history_project(|row, storage| {
                row.ensure_startable()?;
                anyhow::ensure!(
                    row.is_structured()
                        && row.terminal_handoff.is_none()
                        && witness.origin.same_physical_row(row, storage)?
                        && witness.postimage.context == witness.origin.context
                        && witness.postimage.attempt_id == witness.origin.attempt_id
                        && witness.postimage.choice.matches(row)
                        && row.acp_establishment
                            == Some(AcpEstablishment::Applied {
                                attempt_id: witness.origin.attempt_id,
                            }),
                    EstablishmentRefusal::Superseded
                );
                Ok(())
            })?
            .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let owner = witness
            .origin
            .owner
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let registry = crate::process::worker_registry::try_registry_lock(&witness.session_id)?
            .ok_or(crate::process::worker_registry::RegistryBusy)?;
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            registry.stop()?.is_none()
                && owner.birth_fingerprint.is_some()
                && owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(witness.profile.as_str())
                && crate::process::process_birth_identity(owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok((native, registry))
    }

    /// Guard actual tracking-state insertion too, without pretending IO began.
    /// Hold native exclusions before the existing allocation-map lock.
    pub(crate) fn admit_tracking<T>(
        &self,
        insert: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _guard = match &self.source {
            NativeResourceSource::Current(start)
                if !self.started.load(std::sync::atomic::Ordering::Acquire) =>
            {
                start.context.try_effect_guard()?
            }
            _ => self.original_guard()?,
        };
        insert()
    }

    /// Invoke inside real blocking IO or at actual container command spawn.
    /// Only successful execution of that effect establishes Original custody.
    pub(crate) fn with_read<T>(
        &self,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.witness
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let already_owned = self.started.load(std::sync::atomic::Ordering::Acquire);
        let _guard = if already_owned {
            self.original_guard()?
        } else {
            match &self.source {
                NativeResourceSource::Original(_) => self.original_guard()?,
                NativeResourceSource::Current(start) => match start.context.effect_guard() {
                    Ok(guard) => guard,
                    Err(_) if self.started.load(std::sync::atomic::Ordering::Acquire) => {
                        self.original_guard()?
                    }
                    Err(error) => return Err(error),
                },
            }
        };
        let result = effect()?;
        self.started
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(result)
    }

    pub(crate) fn bind_workflow_root(&self, root: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self.selector(),
                NativeResourceSelector::Task {
                    workflow: Some(_),
                    ..
                }
            ) && self.witness().is_some(),
            EstablishmentRefusal::Unconfirmed
        );
        if let Some(existing) = self.workflow_root.get() {
            anyhow::ensure!(existing == root, EstablishmentRefusal::Superseded);
        } else {
            self.workflow_root
                .set(root.to_path_buf())
                .map_err(|_| EstablishmentRefusal::Superseded)?;
        }
        Ok(())
    }

    /// Derive a bounded read-only child of the already-owned workflow journal.
    pub(crate) fn workflow_child(
        &self,
        agent_id: &str,
        output_file: &std::path::Path,
        retained: Option<std::sync::Arc<NativeResourceWitness>>,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        let parent = self.witness().ok_or(EstablishmentRefusal::Unconfirmed)?;
        let NativeResourceSelector::Task {
            task_id,
            workflow: Some(workflow),
            ..
        } = self.selector()
        else {
            anyhow::bail!(EstablishmentRefusal::Unconfirmed);
        };
        let root = self
            .workflow_root
            .get()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            output_file.parent() == Some(root.as_path())
                && output_file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("agent-"))
                    .and_then(|name| name.strip_suffix(".jsonl"))
                    == Some(agent_id),
            EstablishmentRefusal::Superseded
        );
        let output_path = output_file
            .to_str()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let selector = if let Some(witness) = retained.as_ref() {
            let same_selector = matches!(witness.selector.as_ref(),
                NativeResourceSelector::WorkflowChild {
                    parent_allocation, run_id, task_id: selected_task, agent_id: selected_agent,
                    output_file: selected_path, source: NativeResourceLocation::Host,
                } if *parent_allocation == parent.allocation && run_id == &workflow.run_id
                    && selected_task == task_id && selected_agent == agent_id
                    && selected_path == output_path);
            anyhow::ensure!(
                witness.allocation != parent.allocation
                    && same_selector
                    && witness.origin == parent.origin
                    && witness.profile == parent.profile
                    && witness.session_id == parent.session_id
                    && witness.created_at == parent.created_at
                    && witness.native_session_id == parent.native_session_id
                    && witness.postimage == parent.postimage
                    && witness.admitted == parent.admitted,
                EstablishmentRefusal::Superseded
            );
            std::sync::Arc::clone(&witness.selector)
        } else {
            std::sync::Arc::new(NativeResourceSelector::WorkflowChild {
                parent_allocation: parent.allocation,
                run_id: workflow.run_id.clone(),
                task_id: task_id.clone(),
                agent_id: agent_id.to_string(),
                output_file: output_path.to_string(),
                source: NativeResourceLocation::Host,
            })
        };
        let started = retained.is_some();
        Ok(std::sync::Arc::new(Self {
            witness: retained,
            selector,
            source: NativeResourceSource::Original(parent),
            fence: std::sync::Arc::clone(&self.fence),
            started: std::sync::atomic::AtomicBool::new(started),
            workflow_root: std::sync::OnceLock::new(),
        }))
    }

    pub(crate) fn check_transcript(
        &self,
        source: &crate::acp::background_agent::TranscriptSource,
        path: &str,
    ) -> anyhow::Result<()> {
        let (expected, location) = match self.selector() {
            NativeResourceSelector::Agent {
                output_file,
                source,
                ..
            }
            | NativeResourceSelector::WorkflowChild {
                output_file,
                source,
                ..
            } => (output_file, source),
            NativeResourceSelector::Task { .. } => anyhow::bail!(EstablishmentRefusal::Superseded),
        };
        let same_source = match (location, source) {
            (
                NativeResourceLocation::Host,
                crate::acp::background_agent::TranscriptSource::Host,
            ) => true,
            (
                NativeResourceLocation::Container { runtime, container },
                crate::acp::background_agent::TranscriptSource::Container {
                    runtime: actual_runtime,
                    container: actual_container,
                },
            ) => {
                runtime.as_bytes() == actual_runtime.as_bytes()
                    && container.as_bytes() == actual_container.as_bytes()
            }
            _ => false,
        };
        anyhow::ensure!(
            same_source && expected == path,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    pub(crate) fn progress_sender(
        &self,
        events: &crate::acp::event_sender::EventSender,
    ) -> crate::acp::event_sender::EventSender {
        if !self.started.load(std::sync::atomic::Ordering::Acquire) {
            return events.with_metadata(crate::acp::event_sender::EventMetadata::history());
        }
        let Some(witness) = self.witness.as_deref() else {
            return events.with_metadata(crate::acp::event_sender::EventMetadata::history());
        };
        events.with_resource_reference(witness)
    }

    pub(crate) fn start_sender(
        &self,
        events: &crate::acp::event_sender::EventSender,
    ) -> crate::acp::event_sender::EventSender {
        match self.witness() {
            Some(witness) => events.with_resource_witness(witness),
            None => events.with_metadata(crate::acp::event_sender::EventMetadata::history()),
        }
    }
}

impl NativeSessionPermission {
    pub(crate) fn capture(
        receipt: &NativeEstablishmentReceipt,
        row: &Instance,
        storage: &Storage,
        owner: &crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> anyhow::Result<Self> {
        Self::admit_row(row)?;
        Ok(Self {
            postimage: receipt.current_applied_postimage(row, storage, owner)?,
            generation: row.lifecycle_generation,
            profile_identity: storage.bound_profile_identity()?,
            owner: *owner,
        })
    }

    pub(crate) fn capture_stamp(
        receipt: &NativeEstablishmentReceipt,
        postimage: &EstablishmentPostimage,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<NativeSessionStamp> {
        Self::admit_row(row)?;
        anyhow::ensure!(
            receipt.attempt.producer.is_some()
                && receipt.owner.birth_fingerprint.is_some()
                && receipt.session_id().is_some_and(|sid| !sid.is_empty())
                && receipt.replay.closed
                && !receipt.replay.lost,
            EstablishmentRefusal::Unconfirmed
        );
        anyhow::ensure!(
            postimage.attempt_id == receipt.attempt.id
                && postimage.context == receipt.attempt.context
                && receipt.attempt.matches_physical_source(row, storage)?
                && postimage.choice.matches(row)
                && row.acp_establishment
                    == Some(AcpEstablishment::Applied {
                        attempt_id: receipt.attempt.id,
                    }),
            EstablishmentRefusal::Superseded
        );
        Ok(NativeSessionStamp {
            context: NativeEventContext::capture(row, storage)?,
            generation: row.lifecycle_generation,
            profile_identity: storage.bound_profile_identity()?,
        })
    }

    /// Reconstruct only from authenticated, pre-queue native observation.
    /// The unchanged original postimage supplies choice, never a current row.
    pub(crate) fn from_stamp(
        receipt: &NativeEstablishmentReceipt,
        original: &EstablishmentPostimage,
        stamp: NativeSessionStamp,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            original.attempt_id == receipt.attempt.id
                && original.context == receipt.attempt.context
                && receipt.owner.birth_fingerprint.is_some()
                && receipt.replay.closed
                && !receipt.replay.lost,
            EstablishmentRefusal::Unconfirmed
        );
        let mut postimage = original.clone();
        postimage.context = stamp.context;
        Ok(Self {
            postimage,
            generation: stamp.generation,
            profile_identity: stamp.profile_identity,
            owner: receipt.owner,
        })
    }

    pub(crate) fn check_stamp_owner(
        receipt: &NativeEstablishmentReceipt,
        registry: &crate::process::worker_registry::RegistryGuard<'_>,
    ) -> anyhow::Result<()> {
        let owner = receipt.owner;
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            owner.birth_fingerprint.is_some()
                && registry.stop()?.is_none()
                && owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(receipt.attempt.profile.as_str())
                && crate::process::process_birth_identity(owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    fn admit_row(row: &Instance) -> anyhow::Result<()> {
        row.ensure_startable()?;
        anyhow::ensure!(
            row.is_structured()
                && row.terminal_handoff.is_none()
                && !row.is_idle_dormant()
                && !matches!(
                    row.status,
                    crate::session::Status::Creating
                        | crate::session::Status::Deleting
                        | crate::session::Status::Stopped
                )
                && !row.has_fresh_lifecycle_reservation(chrono::Utc::now()),
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    pub(crate) fn matches(
        &self,
        receipt: &NativeEstablishmentReceipt,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        Self::admit_row(row)?;
        Ok(self.generation == row.lifecycle_generation
            && self.profile_identity == storage.bound_profile_identity()?
            && self.postimage == receipt.current_applied_postimage(row, storage, &self.owner)?)
    }

    /// Build the physical route from this permission's frozen generation and
    /// profile identity. Never derive a replacement permission after queueing.
    pub(crate) fn effect_fence(
        &self,
        receipt: &NativeEstablishmentReceipt,
    ) -> anyhow::Result<std::sync::Arc<crate::session::NativeInputFence>> {
        let _effect = self.effect_guard(receipt)?;
        crate::session::NativeInputFence::capture_route(
            &receipt.attempt.profile,
            &receipt.attempt.session_id,
            receipt.attempt.created_at,
            self.generation,
            false,
            Some(self.profile_identity),
        )
    }

    /// Synchronous effect boundary. Callers release both guards before awaits.
    pub(crate) fn effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.lock_effect_guard(receipt, false)
    }

    pub(crate) fn try_effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        self.lock_effect_guard(receipt, true)
    }

    fn lock_effect_guard<'a>(
        &self,
        receipt: &'a NativeEstablishmentReceipt,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'a>,
    )> {
        let storage = Storage::open_reference(&receipt.attempt.profile)?;
        let native = if nonblocking {
            storage
                .try_acquire_instance_lifecycle_lock(&receipt.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?
        } else {
            storage.acquire_instance_lifecycle_lock(&receipt.attempt.session_id)?
        };
        let row = storage
            .load_instances_complete()?
            .into_iter()
            .find(|row| row.id == receipt.attempt.session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            self.matches(receipt, &row, &storage)?,
            EstablishmentRefusal::Superseded
        );
        let registry =
            crate::process::worker_registry::try_registry_lock(&receipt.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        self.check_owner(receipt, &registry)?;
        Ok((native, registry))
    }

    fn check_owner(
        &self,
        receipt: &NativeEstablishmentReceipt,
        registry: &crate::process::worker_registry::RegistryGuard<'_>,
    ) -> anyhow::Result<()> {
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            registry.stop()?.is_none()
                && self.owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(receipt.attempt.profile.as_str())
                && crate::process::process_birth_identity(self.owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == self.owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }
}

impl EstablishmentPostimage {
    pub(crate) fn matches(&self, row: &Instance, storage: &Storage) -> anyhow::Result<bool> {
        Ok(self.context == NativeEventContext::capture(row, storage)?
            && self.choice.matches(row)
            && row.acp_establishment
                == Some(AcpEstablishment::Applied {
                    attempt_id: self.attempt_id,
                }))
    }
}

impl NativeEstablishmentReceipt {
    pub(crate) fn session_id(&self) -> Option<&str> {
        match &self.outcome {
            NativeEstablishmentOutcome::Succeeded { session_id } => Some(session_id),
            NativeEstablishmentOutcome::Failed { .. } => None,
        }
    }

    pub(crate) fn same_store(&self, other: Option<&crate::session::ExecutionBinding>) -> bool {
        self.store.as_ref() == other
            && self
                .store
                .as_ref()
                .map(|store| store.exported_default_store)
                == other.map(|store| store.exported_default_store)
    }
    /// Verify a current Applied choice from immutable original evidence.
    ///
    /// This pure predicate does not certify the receipt or physical custody.
    /// Callers must also prove the original seal/cache and current owner under
    /// their existing lifecycle and registry exclusions.
    pub(crate) fn current_applied_postimage(
        &self,
        row: &Instance,
        storage: &Storage,
        owner: &crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> anyhow::Result<EstablishmentPostimage> {
        anyhow::ensure!(
            self.attempt.producer.is_some()
                && owner.birth_fingerprint.is_some()
                && self.owner == *owner
                && self.session_id().is_some_and(|sid| !sid.is_empty())
                && self.replay.closed
                && !self.replay.lost,
            EstablishmentRefusal::Unconfirmed
        );
        anyhow::ensure!(
            self.attempt.matches_physical_source(row, storage)?,
            EstablishmentRefusal::Superseded
        );
        let postimage = self.postimage(NativeEventContext::capture(row, storage)?)?;
        anyhow::ensure!(
            postimage.matches(row, storage)?,
            EstablishmentRefusal::Superseded
        );
        Ok(postimage)
    }

    pub(crate) fn postimage(
        &self,
        context: NativeEventContext,
    ) -> anyhow::Result<EstablishmentPostimage> {
        let sid = self.session_id().ok_or(EstablishmentRefusal::Unconfirmed)?;
        if let EstablishmentTarget::Load { session_id, .. } = &self.attempt.target {
            anyhow::ensure!(session_id == sid, EstablishmentRefusal::Superseded);
        }
        let mut choice = self.attempt.destination.clone();
        choice.acp_session_id = Some(sid.to_owned());
        choice.fork_pending = None;
        choice.import_pending = None;
        choice.authority = Some(AuthorityKey {
            id: self.attempt.id,
            phase: AuthorityPhase::Applied,
        });
        choice.choice.establish_acp_identity(
            sid,
            self.store.as_ref(),
            choice.sandboxed,
            matches!(self.attempt.target, EstablishmentTarget::Load { .. }),
        );
        Ok(EstablishmentPostimage {
            attempt_id: self.attempt.id,
            context,
            choice,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EstablishmentRefusal {
    #[error(
        "Conversation establishment is unresolved; choose Retry same or Start fresh explicitly"
    )]
    Unresolved,
    #[error("No retained conversation or fresh-start consent was selected")]
    NoSelection,
    #[error("Conversation establishment choice changed")]
    Superseded,
    #[error("Conversation establishment or its replay is not durably confirmed")]
    Unconfirmed,
}

impl EstablishmentAttempt {
    pub(crate) fn same_source(&self, other: &Self) -> bool {
        self.same_attempt(other) && self.producer == other.producer
    }

    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        self.id == other.id
            && self.profile == other.profile
            && self.session_id == other.session_id
            && self.created_at == other.created_at
            && self.source_context == other.source_context
            && self.preimage == other.preimage
            && self.target == other.target
            && match (self.origin, other.origin) {
                (EstablishmentOrigin::Creation { .. }, EstablishmentOrigin::Creation { .. }) => {
                    true
                }
                (left, right) => left == right,
            }
    }

    fn awaits_creation_admission(&self) -> bool {
        self.producer.is_none()
            && matches!(
                self.origin,
                EstablishmentOrigin::Creation {
                    stage: CreationStage::Ready,
                }
            )
    }

    pub(crate) fn allows_effects(&self) -> bool {
        !matches!(
            self.origin,
            EstablishmentOrigin::Creation {
                stage: CreationStage::Preparing
                    | CreationStage::PreparingAdmitted
                    | CreationStage::Ready,
            }
        )
    }

    pub(crate) fn matches_physical_source(
        &self,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        Ok(self.profile == storage.profile()
            && self.session_id == row.id
            && self.created_at == row.created_at
            && self.context
                == NativeEventContext::capture_identity(
                    &row.id,
                    row.created_at,
                    self.generation,
                    storage,
                )?)
    }

    /// Called off the async executor before the original RPC is emitted.
    pub(crate) fn effect_fence(
        &self,
    ) -> anyhow::Result<std::sync::Arc<crate::session::NativeInputFence>> {
        anyhow::ensure!(
            self.producer.is_some() && self.allows_effects(),
            EstablishmentRefusal::Unconfirmed
        );
        let storage = Storage::open_reference(&self.profile)?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock(&self.session_id)?;
        let row = storage
            .load_instances_complete()?
            .into_iter()
            .find(|row| row.id == self.session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            self.matches_pending(&row, &storage)?,
            EstablishmentRefusal::Superseded
        );
        crate::session::NativeInputFence::capture_route(
            &self.profile,
            &self.session_id,
            self.created_at,
            self.generation,
            true,
            Some(storage.bound_profile_identity()?),
        )
    }
    pub(crate) fn applied_fence(
        &self,
        postimage: &EstablishmentPostimage,
    ) -> anyhow::Result<std::sync::Arc<crate::session::NativeInputFence>> {
        anyhow::ensure!(
            self.id == postimage.attempt_id && self.context == postimage.context,
            EstablishmentRefusal::Superseded
        );
        let storage = Storage::open_reference(&self.profile)?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock(&self.session_id)?;
        let row = storage
            .load_instances_complete()?
            .into_iter()
            .find(|row| row.id == self.session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            postimage.matches(&row, &storage)?,
            EstablishmentRefusal::Unresolved
        );
        crate::session::NativeInputFence::capture_route(
            &self.profile,
            &self.session_id,
            self.created_at,
            self.generation,
            false,
            Some(storage.bound_profile_identity()?),
        )
    }

    /// Original source survives generation-only rebases; Current checks stay separate.
    pub(crate) fn matches_pending_source(
        &self,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        Ok(
            matches!(&row.acp_establishment, Some(AcpEstablishment::Pending { attempt })
            if self.same_source(attempt) && self.destination == attempt.destination)
                && self.matches_physical_source(row, storage)?
                && self.destination.matches(row),
        )
    }

    pub(crate) fn matches_pending(
        &self,
        row: &Instance,
        storage: &Storage,
    ) -> anyhow::Result<bool> {
        Ok(
            matches!(&row.acp_establishment, Some(AcpEstablishment::Pending { attempt })
                if attempt.as_ref() == self)
                && self.profile == storage.profile()
                && self.session_id == row.id
                && self.created_at == row.created_at
                && self.generation == row.lifecycle_generation
                && self.context == NativeEventContext::capture(row, storage)?
                && self.destination.matches(row),
        )
    }
}

pub(crate) enum CreationAdmission {
    Later,
    Now,
}

/// Insert a genuinely absent app session and its initial native permission together.
pub(crate) fn insert_created_instance(
    rows: &mut Vec<Instance>,
    row: &mut Instance,
    storage: &Storage,
    admission: CreationAdmission,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !rows.iter().any(|existing| existing.id == row.id),
        EstablishmentRefusal::Superseded
    );
    anyhow::ensure!(
        row.acp_establishment.is_none(),
        EstablishmentRefusal::Superseded
    );
    if row.is_structured() {
        let stage = match admission {
            CreationAdmission::Later => {
                anyhow::ensure!(row.launch_is_finalized(), EstablishmentRefusal::Unresolved);
                CreationStage::Ready
            }
            CreationAdmission::Now => CreationStage::PreparingAdmitted,
        };
        row.arm_establishment(storage, true, None)?;
        let Some(AcpEstablishment::Pending { attempt }) = row.acp_establishment.as_mut() else {
            unreachable!("absent-row creation was armed");
        };
        attempt.origin = EstablishmentOrigin::Creation { stage };
    }
    rows.push(row.clone());
    Ok(())
}

impl Instance {
    pub(crate) fn require_resolved_acp_selection(&self) -> Result<(), EstablishmentRefusal> {
        if !self.is_structured() {
            return Ok(());
        }
        if self.launch_is_finalized()
            && matches!(&self.acp_establishment,
                Some(AcpEstablishment::Pending { attempt })
                    if attempt.awaits_creation_admission())
        {
            return Ok(());
        }
        if self
            .acp_establishment
            .as_ref()
            .is_some_and(AcpEstablishment::unresolved)
        {
            return Err(EstablishmentRefusal::Unresolved);
        }
        self.selected_acp_target().map(|_| ())
    }

    pub(crate) fn selected_acp_target(&self) -> Result<EstablishmentTarget, EstablishmentRefusal> {
        if let Some(parent) = self.fork_pending.as_ref().filter(|id| !id.is_empty()) {
            return Ok(EstablishmentTarget::Fork {
                session_id: parent.clone(),
            });
        }
        match &self.resume_intent {
            crate::session::ResumeIntent::Use(id) if !id.is_empty() => {
                return Ok(EstablishmentTarget::Load {
                    session_id: id.clone(),
                    import: self.import_pending == Some(true),
                });
            }
            crate::session::ResumeIntent::Fork { from } if !from.is_empty() => {
                return Ok(EstablishmentTarget::Fork {
                    session_id: from.clone(),
                });
            }
            crate::session::ResumeIntent::Cleared => return Err(EstablishmentRefusal::NoSelection),
            _ => {}
        }
        self.acp_session_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .map(|id| EstablishmentTarget::Load {
                session_id: id.clone(),
                import: self.import_pending == Some(true),
            })
            .ok_or(EstablishmentRefusal::NoSelection)
    }

    /// Caller owns the native CAS. Only the absent-row factory passes `creation`.
    /// A missing legacy selection never becomes permission to create a conversation.
    pub(crate) fn arm_acp_establishment(
        &mut self,
        storage: &Storage,
        recovery: Option<&EstablishmentRecovery>,
    ) -> anyhow::Result<EstablishmentAttempt> {
        match self.acp_establishment.as_ref() {
            Some(AcpEstablishment::Pending { attempt })
                if recovery.is_none() && attempt.awaits_creation_admission() =>
            {
                anyhow::ensure!(
                    self.launch_is_finalized() && attempt.matches_pending(self, storage)?,
                    EstablishmentRefusal::Superseded
                );
                let Some(AcpEstablishment::Pending { attempt }) = self.acp_establishment.as_mut()
                else {
                    unreachable!("checked pending creation");
                };
                attempt.origin = EstablishmentOrigin::Creation {
                    stage: CreationStage::Admitted,
                };
                Ok(attempt.as_ref().clone())
            }
            _ => self.arm_establishment(storage, false, recovery),
        }
    }

    fn arm_establishment(
        &mut self,
        storage: &Storage,
        creation: bool,
        recovery: Option<&EstablishmentRecovery>,
    ) -> anyhow::Result<EstablishmentAttempt> {
        if let Some(recovery) = recovery {
            anyhow::ensure!(
                recovery.expected.matches(self, storage)?,
                EstablishmentRefusal::Superseded
            );
        } else {
            anyhow::ensure!(
                !self
                    .acp_establishment
                    .as_ref()
                    .is_some_and(AcpEstablishment::unresolved),
                EstablishmentRefusal::Unresolved
            );
        }
        let original_target =
            if recovery.is_some_and(|r| r.action == EstablishmentRecoveryAction::Same) {
                self.acp_establishment
                    .as_ref()
                    .and_then(AcpEstablishment::attempt)
                    .map(|attempt| {
                        anyhow::ensure!(
                            attempt.profile == storage.profile()
                                && attempt.session_id == self.id
                                && attempt.created_at == self.created_at
                                && attempt.generation == self.lifecycle_generation
                                && attempt.context == NativeEventContext::capture(self, storage)?
                                && attempt.destination.selection_matches(self),
                            EstablishmentRefusal::Superseded
                        );
                        Ok::<_, anyhow::Error>(attempt.target.clone())
                    })
                    .transpose()?
            } else {
                None
            };
        let preimage = NativeBindingState::capture(self);
        let unissued_new = recovery.is_some_and(|recovery| {
            recovery.action == EstablishmentRecoveryAction::Same
                && self
                    .acp_establishment
                    .as_ref()
                    .and_then(AcpEstablishment::attempt)
                    .is_some_and(|attempt| {
                        attempt.target == EstablishmentTarget::New
                            && (attempt.producer.is_none()
                                || matches!(
                                    self.acp_establishment,
                                    Some(AcpEstablishment::Blocked {
                                        reason: EstablishmentBlockReason::NotIssued,
                                        ..
                                    })
                                ))
                    })
        });
        let creation_new = creation
            && self.acp_session_id.is_none()
            && self.fork_pending.is_none()
            && self.import_pending != Some(true)
            && matches!(
                self.resume_intent,
                crate::session::ResumeIntent::Default | crate::session::ResumeIntent::Cleared
            );
        let fresh = creation_new
            || unissued_new
            || recovery.is_some_and(|r| r.action == EstablishmentRecoveryAction::Fresh);
        if fresh {
            self.clear_acp_selection_for_fresh();
        }
        let target = if fresh {
            EstablishmentTarget::New
        } else {
            match original_target {
                Some(target) => {
                    anyhow::ensure!(
                        target != EstablishmentTarget::New,
                        EstablishmentRefusal::Unresolved
                    );
                    target
                }
                None => self.selected_acp_target()?,
            }
        };
        let context = NativeEventContext::capture(self, storage)?;
        let id = AttemptId(Uuid::new_v4());
        let mut destination = NativeBindingState::capture(self);
        destination.authority = Some(AuthorityKey {
            id,
            phase: AuthorityPhase::Pending,
        });
        let attempt = EstablishmentAttempt {
            id,
            profile: storage.profile().to_owned(),
            session_id: self.id.clone(),
            created_at: self.created_at,
            source_context: context,
            context,
            generation: self.lifecycle_generation,
            preimage,
            destination,
            target,
            origin: if creation {
                EstablishmentOrigin::Creation {
                    stage: CreationStage::Preparing,
                }
            } else {
                match recovery.map(|recovery| recovery.action) {
                    Some(EstablishmentRecoveryAction::Same) => EstablishmentOrigin::Same,
                    Some(EstablishmentRecoveryAction::Fresh) => EstablishmentOrigin::Fresh,
                    None => EstablishmentOrigin::Selection,
                }
            },
            producer: None,
        };
        self.acp_establishment = Some(AcpEstablishment::Pending {
            attempt: Box::new(attempt.clone()),
        });
        Ok(attempt)
    }
    fn clear_acp_selection_for_fresh(&mut self) {
        self.acp_session_id = None;
        self.fork_pending = None;
        self.import_pending = None;
        self.set_agent_conversation(None, None, None);
        self.resume_binding = None;
        self.active_execution = None;
        self.resume_intent = crate::session::ResumeIntent::Cleared;
    }

    /// Install one already captured proposal; never allocate another attempt id.
    pub(crate) fn install_prepared_acp_fresh(
        &mut self,
        storage: &Storage,
        expected: &EstablishmentExpected,
        proposed: &EstablishmentAttempt,
        producer: Uuid,
    ) -> anyhow::Result<()> {
        self.ensure_startable()?;
        anyhow::ensure!(
            expected.matches(self, storage)?
                && self.is_structured()
                && self.launch_is_finalized()
                && self.terminal_handoff.is_none()
                && proposed.profile == storage.profile()
                && proposed.session_id == self.id
                && proposed.created_at == self.created_at
                && proposed.generation == self.lifecycle_generation
                && proposed.context == NativeEventContext::capture(self, storage)?
                && proposed.source_context == proposed.context
                && proposed.preimage == expected.choice
                && proposed.origin == EstablishmentOrigin::Fresh
                && proposed.target == EstablishmentTarget::New
                && proposed.producer == Some(producer)
                && matches!(self.acp_establishment,
                    Some(AcpEstablishment::Applied { attempt_id }) if attempt_id != proposed.id),
            EstablishmentRefusal::Superseded
        );
        self.clear_acp_selection_for_fresh();
        self.acp_establishment = Some(AcpEstablishment::Pending {
            attempt: Box::new(proposed.clone()),
        });
        anyhow::ensure!(
            proposed.matches_pending(self, storage)?,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    /// Only the exact creating controller may finish setup; reservation expiry is
    /// not this transition. The caller commits this with its final setup fields.
    pub(crate) fn finish_acp_creation(
        &mut self,
        storage: &Storage,
        expected: &EstablishmentAttempt,
    ) -> anyhow::Result<EstablishmentAttempt> {
        anyhow::ensure!(
            expected.matches_pending(self, storage)? && expected.producer.is_none(),
            EstablishmentRefusal::Superseded
        );
        let stage = match expected.origin {
            EstablishmentOrigin::Creation {
                stage: CreationStage::Preparing,
            } => CreationStage::Ready,
            EstablishmentOrigin::Creation {
                stage: CreationStage::PreparingAdmitted,
            } => CreationStage::Admitted,
            _ => anyhow::bail!(EstablishmentRefusal::Superseded),
        };
        let mut attempt = expected.clone();
        attempt.origin = EstablishmentOrigin::Creation { stage };
        self.acp_establishment = Some(AcpEstablishment::Pending {
            attempt: Box::new(attempt.clone()),
        });
        Ok(attempt)
    }
    /// A qualified terminal fallback discards the unused ACP creation permission
    /// before any terminal agent starts. It is never a reusable ACP New marker.
    pub(crate) fn discard_acp_creation(
        &mut self,
        storage: &Storage,
        expected: &EstablishmentAttempt,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            expected.matches_pending(self, storage)?
                && expected.producer.is_none()
                && matches!(
                    expected.origin,
                    EstablishmentOrigin::Creation {
                        stage: CreationStage::Preparing | CreationStage::PreparingAdmitted,
                    }
                ),
            EstablishmentRefusal::Superseded
        );
        self.acp_establishment = None;
        Ok(())
    }

    pub(crate) fn rebase_acp_generation(&mut self, storage: &Storage) -> anyhow::Result<()> {
        let Some(expected) = self
            .acp_establishment
            .as_ref()
            .and_then(AcpEstablishment::attempt)
        else {
            return Ok(());
        };
        anyhow::ensure!(
            expected.matches_physical_source(self, storage)?
                && expected.destination.selection_matches(self),
            EstablishmentRefusal::Superseded
        );
        let context = NativeEventContext::capture(self, storage)?;
        match self.acp_establishment.as_mut() {
            Some(
                AcpEstablishment::Pending { attempt } | AcpEstablishment::Blocked { attempt, .. },
            ) => {
                attempt.context = context;
                attempt.generation = self.lifecycle_generation;
            }
            _ => unreachable!("checked unresolved establishment"),
        }
        Ok(())
    }

    pub(crate) fn block_acp_establishment(
        &mut self,
        storage: &Storage,
        expected: &EstablishmentAttempt,
        reason: EstablishmentBlockReason,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            expected.matches_pending(self, storage)?,
            EstablishmentRefusal::Superseded
        );
        let mut attempt = expected.clone();
        attempt.destination.authority = Some(AuthorityKey {
            id: attempt.id,
            phase: AuthorityPhase::Blocked,
        });
        self.acp_establishment = Some(AcpEstablishment::Blocked {
            attempt: Box::new(attempt),
            reason,
        });
        Ok(())
    }

    pub(crate) fn bind_acp_producer(
        &mut self,
        storage: &Storage,
        expected: &EstablishmentAttempt,
        producer: Uuid,
    ) -> anyhow::Result<EstablishmentAttempt> {
        anyhow::ensure!(
            expected.matches_pending(self, storage)?,
            EstablishmentRefusal::Superseded
        );
        anyhow::ensure!(expected.allows_effects(), EstablishmentRefusal::Unresolved);
        let Some(AcpEstablishment::Pending { attempt }) = &mut self.acp_establishment else {
            unreachable!("checked Pending");
        };
        anyhow::ensure!(
            attempt.producer.is_none_or(|original| original == producer),
            EstablishmentRefusal::Superseded
        );
        attempt.producer = Some(producer);
        Ok(attempt.as_ref().clone())
    }

    pub(crate) fn commit_acp_establishment(
        &mut self,
        storage: &Storage,
        receipt: &NativeEstablishmentReceipt,
        seal: &crate::acp::event_store::EstablishmentSeal,
        destination: NativeEventContext,
    ) -> anyhow::Result<EstablishmentPostimage> {
        let expected = &receipt.attempt;
        anyhow::ensure!(
            receipt.owner.birth_fingerprint.is_some(),
            EstablishmentRefusal::Unconfirmed
        );
        anyhow::ensure!(seal.matches(receipt), EstablishmentRefusal::Unconfirmed);
        anyhow::ensure!(
            expected.matches_physical_source(self, storage)?,
            EstablishmentRefusal::Superseded
        );
        if self.acp_establishment
            == Some(AcpEstablishment::Applied {
                attempt_id: expected.id,
            })
        {
            let postimage = receipt.postimage(destination)?;
            anyhow::ensure!(
                postimage.matches(self, storage)?,
                EstablishmentRefusal::Superseded
            );
            return Ok(postimage);
        }
        let pending = match &self.acp_establishment {
            Some(AcpEstablishment::Pending { attempt }) => attempt,
            _ => anyhow::bail!(EstablishmentRefusal::Superseded),
        };
        anyhow::ensure!(
            expected.same_source(pending)
                && pending.context == destination
                && pending.context == NativeEventContext::capture(self, storage)?
                && pending.generation == self.lifecycle_generation
                && pending.destination == expected.destination
                && pending.destination.matches(self),
            EstablishmentRefusal::Superseded
        );
        let postimage = receipt.postimage(destination)?;
        let choice = &postimage.choice.choice;
        self.acp_session_id = postimage.choice.acp_session_id.clone();
        self.fork_pending = None;
        self.import_pending = None;
        self.set_agent_conversation(
            choice.session_id.clone(),
            choice.binding.clone(),
            choice.pi_session_path.clone(),
        );
        self.resume_binding = choice.resume_binding.clone();
        self.resume_intent = choice.intent.clone();
        self.acp_establishment = Some(AcpEstablishment::Applied {
            attempt_id: expected.id,
        });
        Ok(postimage)
    }

    pub(crate) fn validate_acp_establishment(&self, storage: &Storage) -> anyhow::Result<()> {
        let Some(authority) = &self.acp_establishment else {
            return Ok(());
        };
        let Some(attempt) = authority.attempt() else {
            anyhow::ensure!(
                self.acp_session_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()),
                "Applied ACP establishment has no selected identity"
            );
            anyhow::ensure!(
                self.fork_pending.is_none() && self.import_pending != Some(true),
                "Applied ACP establishment retains unfinished selection"
            );
            return Ok(());
        };
        anyhow::ensure!(
            attempt.profile == storage.profile()
                && attempt.session_id == self.id
                && attempt.created_at == self.created_at
                && attempt.generation == self.lifecycle_generation
                && attempt.context == NativeEventContext::capture(self, storage)?
                && attempt.destination.matches(self)
                && attempt.destination.authority == Some(authority.key())
                && attempt
                    .preimage
                    .authority
                    .is_none_or(|key| key.id != attempt.id),
            "ACP establishment scope or selection is inconsistent"
        );
        match &attempt.target {
            EstablishmentTarget::New => anyhow::ensure!(
                self.acp_session_id.is_none()
                    && self.fork_pending.is_none()
                    && self.import_pending != Some(true),
                "New ACP establishment retains a different target"
            ),
            target => anyhow::ensure!(
                self.selected_acp_target()? == *target,
                "ACP establishment target contradicts its selection"
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::EstablishmentTarget;
    use serde_json::json;

    #[test]
    fn native_result_identity_obeys_the_selected_target() {
        let load = || EstablishmentTarget::Load {
            session_id: "existing".into(),
            import: false,
        };
        for (target, result, expected) in [
            (
                EstablishmentTarget::New,
                json!({"sessionId":"new"}),
                Some("new"),
            ),
            (EstablishmentTarget::New, json!({}), None),
            (EstablishmentTarget::New, json!({"sessionId":""}), None),
            (EstablishmentTarget::New, json!({"sessionId":42}), None),
            (
                EstablishmentTarget::Fork {
                    session_id: "parent".into(),
                },
                json!({"sessionId":"child"}),
                Some("child"),
            ),
            (
                EstablishmentTarget::Fork {
                    session_id: "parent".into(),
                },
                json!({}),
                None,
            ),
            (load(), json!({}), Some("existing")),
            (load(), json!({"sessionId":"existing"}), Some("existing")),
            (load(), json!({"sessionId":"different"}), None),
            (load(), json!({"sessionId":42}), None),
            (load(), json!(null), None),
        ] {
            assert_eq!(
                target.resulting_session_id(&result).ok(),
                expected,
                "{target:?}: {result}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn deferred_creation_admits_its_first_original_once_without_replacing_consent() {
        use super::{
            insert_created_instance, AcpEstablishment, CreationAdmission, CreationStage,
            EstablishmentBlockReason, EstablishmentOrigin,
        };
        use crate::session::{Instance, ResumeIntent, Status, Storage, View};

        let home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("default").unwrap();
        let mut row = Instance::new("deferred-creation", home.path().to_str().unwrap());
        row.view = View::Structured;
        let mut rows = Vec::new();
        row.status = Status::Creating;
        assert!(
            insert_created_instance(&mut rows, &mut row, &storage, CreationAdmission::Later,)
                .is_err()
        );
        assert!(rows.is_empty());
        assert!(row.acp_establishment.is_none());

        row.status = Status::Idle;
        insert_created_instance(&mut rows, &mut row, &storage, CreationAdmission::Later).unwrap();
        let original = row
            .acp_establishment
            .as_ref()
            .and_then(AcpEstablishment::attempt)
            .unwrap()
            .clone();
        for case in [
            "ready",
            "not-finalized",
            "new-generation",
            "different-selection",
            "preparing",
            "already-admitted",
            "blocked",
            "missing-permission",
        ] {
            let mut candidate = row.clone();
            match case {
                "ready" => {}
                "not-finalized" => candidate.status = Status::Creating,
                "new-generation" => candidate.lifecycle_generation += 1,
                "different-selection" => {
                    candidate.resume_intent = ResumeIntent::Use("another-conversation".into());
                }
                "preparing" | "already-admitted" => {
                    let Some(AcpEstablishment::Pending { attempt }) =
                        candidate.acp_establishment.as_mut()
                    else {
                        unreachable!();
                    };
                    attempt.origin = EstablishmentOrigin::Creation {
                        stage: if case == "preparing" {
                            CreationStage::Preparing
                        } else {
                            CreationStage::Admitted
                        },
                    };
                }
                "blocked" => candidate
                    .block_acp_establishment(
                        &storage,
                        &original,
                        EstablishmentBlockReason::OutcomeUnknown,
                    )
                    .unwrap(),
                "missing-permission" => candidate.acp_establishment = None,
                _ => unreachable!(),
            }
            if case == "ready" {
                candidate.require_resolved_acp_selection().unwrap();
                let admitted = candidate.arm_acp_establishment(&storage, None).unwrap();
                assert_eq!(admitted.id, original.id);
                assert_eq!(admitted.target, EstablishmentTarget::New);
                assert_eq!(admitted.source_context, original.source_context);
                assert!(admitted.allows_effects());
                assert!(candidate.arm_acp_establishment(&storage, None).is_err());
                let producer = uuid::Uuid::new_v4();
                let bound = candidate
                    .bind_acp_producer(&storage, &admitted, producer)
                    .unwrap();
                assert!(candidate
                    .bind_acp_producer(&storage, &bound, uuid::Uuid::new_v4())
                    .is_err());
                assert!(candidate.arm_acp_establishment(&storage, None).is_err());
            } else {
                let before = candidate.acp_establishment.clone();
                assert!(
                    candidate.arm_acp_establishment(&storage, None).is_err(),
                    "{case}"
                );
                assert_eq!(candidate.acp_establishment, before, "{case}");
            }
        }
    }
}
